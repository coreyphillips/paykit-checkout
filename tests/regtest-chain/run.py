"""Run from any directory with Python 3, Cargo, and Docker Compose available.

    python3 tests/regtest-chain/run.py

The test creates fresh regtest wallets, a loopback indexer, and a temporary
invoice database. Docker removes the isolated project's containers and volumes
when the test exits. Existing node configurations and wallets are never read.
"""

import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import tempfile
import time
import uuid


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def run(args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def exercise(compose, binary, directory):
    database = directory / "checkout.sqlite3"
    env = {k: v for k, v in os.environ.items() if not k.startswith(("PAYKIT_", "CHECKOUT_"))}

    def checkout(*args):
        return run([binary, "--database", database, "--network", "regtest", *args], env=env, stderr=subprocess.PIPE)

    def bitcoin(*args, wallet=None):
        prefix = [*compose, "exec", "-T", "bitcoin", "bitcoin-cli", "-regtest", "-rpcwait",
                  "-rpcwaittimeout=30", "-rpcport=18443", "-rpcuser=checkout", "-rpcpassword=regtest-fixture"]
        if wallet:
            prefix.append("-rpcwallet=" + wallet)
        return run([*prefix, *args])

    bitcoin("createwallet", "miner")
    bitcoin("createwallet", "merchant")
    descriptors = json.loads(bitcoin("listdescriptors", wallet="merchant"))["descriptors"]
    receiving = next(d["desc"] for d in descriptors if d["active"] and not d["internal"] and d["desc"].startswith("wpkh("))
    account = re.search(r"(tpub[1-9A-HJ-NP-Za-km-z]+)", receiving).group(1)
    mining_address = bitcoin("getnewaddress", wallet="miner")

    def mine(count):
        return json.loads(bitcoin("generatetoaddress", str(count), mining_address))

    def issue(order, amount):
        path = checkout("invoice", "--order-id", order, "--amount-sats", str(amount), "--account-xpub", account)
        invoice_id = path.rsplit("/", 1)[1]
        with sqlite3.connect(database) as db:
            address, = db.execute("SELECT address FROM invoices WHERE id=?", (invoice_id,)).fetchone()
        return invoice_id, address

    def wait_for(invoice_id, expected, observed, confirmed):
        deadline = time.monotonic() + 60
        actual, error = None, ""
        while time.monotonic() < deadline:
            try:
                endpoint = run([*compose, "port", "electrs", "60001"])
                env["CHECKOUT_ELECTRUM_URL"] = "tcp://" + endpoint
                checkout("worker", "--min-confirmations", "2", "--once")
                status = json.loads(checkout("status", invoice_id))["status"]
                rail = next((r for r in status["rails"] if r["rail"] == "bitcoin"), {})
                actual = status["state"], rail.get("observed_sats", 0), rail.get("confirmed_sats", 0)
                if actual == (expected, observed, confirmed):
                    print(f"{expected}: {observed} observed, {confirmed} confirmed", flush=True)
                    return
            except subprocess.CalledProcessError as exc:
                error = exc.stderr or str(exc)
            time.sleep(1)
        raise AssertionError(f"expected {(expected, observed, confirmed)}, got {actual}. {error}")

    order, address = issue("coffee", 10_000)
    late, late_address = issue("bread", 5000)
    with sqlite3.connect(database) as db:
        # Set the disposable invoice's deadline in the past to avoid a one-hour test.
        db.execute("UPDATE invoices SET expires_at=1 WHERE id=?", (late,))
    mine(101)
    wait_for(order, "unpaid", 0, 0)
    wait_for(late, "expired", 0, 0)

    bitcoin("sendtoaddress", address, "0.00004000", wallet="miner")
    wait_for(order, "partial", 4000, 0)
    mine(2)
    wait_for(order, "partial", 4000, 4000)
    txid = bitcoin("sendtoaddress", address, "0.00006000", wallet="miner")
    bitcoin("sendtoaddress", late_address, "0.00005000", wallet="miner")
    wait_for(order, "confirming", 10_000, 4000)
    wait_for(late, "late", 5000, 0)
    blocks = mine(2)
    wait_for(order, "settled", 10_000, 10_000)
    wait_for(late, "late", 5000, 5000)

    bitcoin("invalidateblock", blocks[0])
    for _ in range(3):
        bitcoin("generateblock", mining_address, "[]")
    wait_for(order, "confirming", 10_000, 4000)
    mine(2)
    wait_for(order, "settled", 10_000, 10_000)

    funding = json.loads(bitcoin("getrawtransaction", txid, "true"))
    vout = next(out["n"] for out in funding["vout"] if out["scriptPubKey"].get("address") == address)
    spend = bitcoin("createrawtransaction", json.dumps([{"txid": txid, "vout": vout}]),
                    json.dumps([{mining_address: "0.00005000"}]))
    signed = json.loads(bitcoin("signrawtransactionwithwallet", spend, wallet="merchant"))
    assert signed["complete"]
    spent = bitcoin("sendrawtransaction", signed["hex"])
    mine(2)
    assert json.loads(bitcoin("getrawtransaction", spent, "true"))["confirmations"] >= 2
    wait_for(order, "settled", 10_000, 10_000)
    with sqlite3.connect(database) as db:
        events = [r[0] for r in db.execute("SELECT kind FROM payment_events WHERE invoice_id=? ORDER BY seq", (order,))]
    assert events.count("invoice.settled") == 1, events
    assert "invoice.settlement_changed" in events and "invoice.restored" in events, events


if __name__ == "__main__":
    name = "checkout-chain-" + uuid.uuid4().hex[:12]
    compose = ["docker", "compose", "-p", name, "-f", HERE / "compose.yaml"]
    subprocess.run(["cargo", "build", "--locked"], cwd=ROOT, check=True)
    metadata = json.loads(run(["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd=ROOT))
    binary = Path(metadata["target_directory"]) / "debug" / "paykit-checkout"
    with tempfile.TemporaryDirectory(prefix=name) as temp:
        try:
            subprocess.run([*compose, "up", "-d"], check=True)
            exercise(compose, binary, Path(temp))
            print("Verified split and late payments, reorganization, spent outputs, and one fulfillment.")
        finally:
            subprocess.run([*compose, "down", "-v"], check=True)
