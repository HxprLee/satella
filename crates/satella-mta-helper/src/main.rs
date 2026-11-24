//! Narrow privileged helper for the Satella MTA sender.
//!
//! This program performs exactly two operations and exits: create or destroy
//! the Wi-Fi Direct group owner. It has no command that runs an arbitrary
//! program, and it refuses to run without root. The Satella polkit action
//! binds `pkexec` to this binary, so the unprivileged CLI never needs to talk
//! to `wpa_supplicant` or run `ip`/`dnsmasq` itself.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use satella_mta_vendor::wifi::p2p_go::{
    configure_go_interface, is_root, remove_go_interface_address, validate_frequency,
    validate_interface_name, DhcpServer, HelperGroupInfo, WpaP2pGroupOwner,
};

const USAGE: &str = "usage:\n  satella-mta-helper create --iface <wlan*|p2p-*> --freq <MHz>\n  satella-mta-helper destroy --iface <wlan*|p2p-*> [--dhcp-pid <pid>]";

/// How long `wpa_supplicant` may take to bring the group up.
const CREATE_TIMEOUT: Duration = Duration::from_secs(20);

enum Command {
    Create {
        interface: String,
        frequency: u32,
    },
    Destroy {
        interface: String,
        dhcp_pid: Option<u32>,
    },
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("satella-mta-helper: {err:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    if !is_root() {
        bail!("must run as root; invoke it through pkexec or the Satella polkit action");
    }

    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| anyhow::anyhow!("arguments must be valid UTF-8"))
        })
        .collect::<Result<_>>()?;

    match parse_args(&args)? {
        Command::Create {
            interface,
            frequency,
        } => {
            let info = create(&interface, frequency).await?;
            println!("{}", serde_json::to_string(&info)?);
        }
        Command::Destroy {
            interface,
            dhcp_pid,
        } => {
            destroy(&interface, dhcp_pid).await?;
            println!("{}", serde_json::json!({ "ok": true }));
        }
    }

    Ok(())
}

fn parse_args(args: &[String]) -> Result<Command> {
    let Some((command, rest)) = args.split_first() else {
        bail!("missing command\n{USAGE}");
    };

    match command.as_str() {
        "create" => {
            let mut flags = parse_flags(rest, &["--iface", "--freq"])?;
            let interface = take(&mut flags, "--iface")?;
            validate_interface_name(&interface)?;

            let frequency = take(&mut flags, "--freq")?
                .parse::<u32>()
                .context("--freq must be a number")?;
            validate_frequency(frequency)?;

            Ok(Command::Create {
                interface,
                frequency,
            })
        }
        "destroy" => {
            let mut flags = parse_flags(rest, &["--iface", "--dhcp-pid"])?;
            let interface = take(&mut flags, "--iface")?;
            validate_interface_name(&interface)?;

            let dhcp_pid = flags
                .remove("--dhcp-pid")
                .map(|value| value.parse::<u32>().context("--dhcp-pid must be a number"))
                .transpose()?;

            Ok(Command::Destroy {
                interface,
                dhcp_pid,
            })
        }
        other => bail!("unknown command {other:?}\n{USAGE}"),
    }
}

fn parse_flags(args: &[String], allowed: &[&str]) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    let mut iterator = args.iter();

    while let Some(flag) = iterator.next() {
        if !allowed.contains(&flag.as_str()) {
            bail!("unknown argument {flag:?}\n{USAGE}");
        }

        let value = iterator
            .next()
            .ok_or_else(|| anyhow::anyhow!("{flag} needs a value\n{USAGE}"))?;

        if value.starts_with("--") {
            bail!("{flag} needs a value\n{USAGE}");
        }

        if values.insert(flag.clone(), value.clone()).is_some() {
            bail!("{flag} was given more than once\n{USAGE}");
        }
    }

    Ok(values)
}

fn take(flags: &mut HashMap<String, String>, name: &str) -> Result<String> {
    flags
        .remove(name)
        .ok_or_else(|| anyhow::anyhow!("missing {name}\n{USAGE}"))
}

async fn create(interface: &str, frequency: u32) -> Result<HelperGroupInfo> {
    let owner = WpaP2pGroupOwner::new(interface).await?;
    let group = owner.create_group(frequency, CREATE_TIMEOUT).await?;

    if let Err(err) = validate_interface_name(&group.interface) {
        let _ = owner.remove_group().await;
        return Err(err);
    }

    if group.ssid.is_empty() || group.psk.is_empty() {
        let _ = owner.remove_group().await;
        bail!("the P2P group has an empty SSID or passphrase");
    }

    if let Err(err) = configure_go_interface(&group.interface).await {
        let _ = remove_go_interface_address(&group.interface).await;
        let _ = owner.remove_group().await;
        return Err(err);
    }

    let dhcp = match DhcpServer::start(&group.interface).await {
        Ok(dhcp) => dhcp,
        Err(err) => {
            let _ = remove_go_interface_address(&group.interface).await;
            let _ = owner.remove_group().await;
            return Err(err);
        }
    };

    Ok(HelperGroupInfo {
        ssid: group.ssid,
        psk: group.psk,
        interface: group.interface,
        frequency: group.frequency,
        dhcp_pid: dhcp.pid(),
    })
}

async fn destroy(interface: &str, expected_pid: Option<u32>) -> Result<()> {
    let mut first_error = None;

    if let Err(err) = DhcpServer::stop_pid(interface, expected_pid).await {
        first_error = Some(err);
    }

    match WpaP2pGroupOwner::new(interface).await {
        Ok(owner) => {
            if let Err(err) = owner.remove_group().await {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        Err(err) => {
            // The interface may already be gone; the address cleanup below is
            // still worth attempting.
            eprintln!("satella-mta-helper: could not reconnect to wpa_supplicant: {err:#}");
        }
    }

    if let Err(err) = remove_go_interface_address(interface).await {
        if first_error.is_none() {
            first_error = Some(err);
        }
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn create_accepts_the_documented_arguments() {
        match parse_args(&args(&["create", "--iface", "wlan0", "--freq", "5745"])) {
            Ok(Command::Create {
                interface,
                frequency,
            }) => {
                assert_eq!(interface, "wlan0");
                assert_eq!(frequency, 5745);
            }
            _ => panic!("create arguments were rejected"),
        }
    }

    #[test]
    fn destroy_accepts_an_optional_pid() {
        match parse_args(&args(&["destroy", "--iface", "wlan0", "--dhcp-pid", "42"])) {
            Ok(Command::Destroy {
                interface,
                dhcp_pid,
            }) => {
                assert_eq!(interface, "wlan0");
                assert_eq!(dhcp_pid, Some(42));
            }
            _ => panic!("destroy arguments were rejected"),
        }
    }

    #[test]
    fn unknown_command_is_rejected() {
        assert!(parse_args(&args(&["shell", "--iface", "wlan0"])).is_err());
    }

    #[test]
    fn unsafe_interface_is_rejected() {
        assert!(parse_args(&args(&["create", "--iface", "eth0", "--freq", "2412"])).is_err());
        assert!(parse_args(&args(&["create", "--iface", "wlan0;id", "--freq", "2412"])).is_err());
    }

    #[test]
    fn out_of_band_frequency_is_rejected() {
        assert!(parse_args(&args(&["create", "--iface", "wlan0", "--freq", "1000"])).is_err());
    }

    #[test]
    fn duplicate_flag_is_rejected() {
        assert!(parse_args(&args(&[
            "create", "--iface", "wlan0", "--iface", "wlan1", "--freq", "2412",
        ]))
        .is_err());
    }

    #[test]
    fn missing_value_is_rejected() {
        assert!(parse_args(&args(&["create", "--iface", "wlan0", "--freq"])).is_err());
    }
}
