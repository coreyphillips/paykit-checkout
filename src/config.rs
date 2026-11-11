use anyhow::{Context, Result, ensure};
use bitcoin::Network;
use reqwest::Url;
use std::{env, fs, path::Path};

pub fn optional(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(v) => {
            ensure!(!v.is_empty(), "{name} is empty");
            Ok(Some(v))
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {name}")),
    }
}

pub fn secret(path: &Path) -> Result<String> {
    let data = fs::read_to_string(path).context("read credential file")?;
    let value = data.trim_end_matches(['\n', '\r']);
    ensure!(
        value.len() >= 32 && value.bytes().all(|b| b.is_ascii_graphic()),
        "credential must contain at least 32 printable ASCII characters"
    );
    Ok(value.to_owned())
}

/// Plain HTTP is accepted only for development services bound to loopback.
pub fn service_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    ensure!(
        url.scheme() == "https" || (loopback && url.scheme() == "http"),
        "use HTTPS or a loopback HTTP URL"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "unexpected URL credentials or fragment"
    );
    Ok(url)
}

pub fn parse_network(value: &str) -> Result<Network> {
    match value {
        "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
        "regtest" => Ok(Network::Regtest),
        _ => anyhow::bail!("expected mainnet or regtest"),
    }
}
