"""Run the checkout client and a real SDK payer on disposable Bitcoin regtest."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
REVISION = "4b0f971f7c24999e78b0bf39fe13424d14b387d5"
ARCHIVE_SHA256 = "a3a38155d16af814c459a27c64f43866767b610f64ed55cc375ee5581e487af2"


def checkout_tests():
    result = subprocess.run(
        ["cargo", "test", "--locked", "--no-run", "--message-format=json"],
        cwd=ROOT, check=True, stdout=subprocess.PIPE, text=True,
    )
    artifacts = (json.loads(line) for line in result.stdout.splitlines())
    return next(
        msg["executable"] for msg in artifacts
        if msg.get("executable") and msg["target"]["name"] == "paykit-checkout"
        and msg["profile"]["test"]
    )


def run():
    binary = checkout_tests()
    with tempfile.TemporaryDirectory(prefix="paykit-checkout-regtest-") as temp:
        work = Path(temp)
        archive = work / "upstream.tar.gz"
        subprocess.run(
            ["curl", "--fail", "--silent", "--show-error", "--location", "--output", archive,
             f"https://codeload.github.com/pubky/paykit-server/tar.gz/{REVISION}"],
            check=True,
        )
        if hashlib.sha256(archive.read_bytes()).hexdigest() != ARCHIVE_SHA256:
            raise SystemExit("Unexpected Paykit Server source archive digest")
        subprocess.run(["tar", "-xzf", archive, "--strip-components", "1", "-C", work], check=True)
        with (HERE / "checkout-driver.patch").open("rb") as changes:
            subprocess.run(["patch", "-p1"], cwd=work, stdin=changes, check=True)
        env = dict(os.environ, CHECKOUT_REGTEST_TEST_BINARY=binary,
                   CHECKOUT_REGTEST_SIGNING_FILE=str(work / "disposable-signing-key"),
                   CHECKOUT_REGTEST_REFERENCE_FILE=str(work / "test-reference"))
        subprocess.run(["./sdk-example/run.sh"], cwd=work, env=env, check=True)


if __name__ == "__main__":
    run()
