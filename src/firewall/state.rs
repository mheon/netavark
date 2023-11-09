use std::{
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;

use crate::{
    error::{NetavarkError, NetavarkResult},
    network::internal_types::{PortForwardConfig, PortForwardConfigOwned, SetupNetwork},
};

/// File layout looks like this
/// $config/firewall/
///                 - firewall-driver -> name of the firewall driver
///                 - networks/$netID -> network config setup
///                 - ports/$netID_$conID -> port config

const FIREWALL_DIR: &str = "firewall";
const FIREWALL_DRIVER_FILE: &str = "firewall-driver";
const NETWORK_CONF_DIR: &str = "networks";
const PORT_CONF_DIR: &str = "ports";

struct FilePaths {
    fw_driver_file: PathBuf,
    net_conf_file: PathBuf,
    port_conf_file: PathBuf,
}

/// macro to quickly wrap the IO error with useful context
/// First argument is the function, second the path, third the extra error message.
/// The full error is "$msg $path: $org_error"
macro_rules! fs_err {
    ($func:expr, $path:expr, $msg:expr) => {
        $func($path).map_err(|err| {
            NetavarkError::wrap(format!("{} {:?}", $msg, $path.display()), err.into())
        })
    };
}

fn remove_file_ignore_enoent<P: AsRef<Path>>(path: P) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(ok) => Ok(ok),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn firewall_config_dir(config_dir: &str) -> PathBuf {
    Path::new(config_dir).join(FIREWALL_DIR)
}

/// Assemble file paths for the config files, when create_dirs is set to true
/// it will create the parent dirs as well so the caller does not have to.
///
/// As a special case when network_id and container_id is empty it will return
/// the paths for the directories instead which are used to walk the dir for all configs.
fn get_file_paths(
    config_dir: &str,
    network_id: &str,
    container_id: &str,
    create_dirs: bool,
) -> NetavarkResult<FilePaths> {
    let path = firewall_config_dir(config_dir);
    let fw_driver_file = path.join(FIREWALL_DRIVER_FILE);
    let mut net_conf_file = path.join(NETWORK_CONF_DIR);
    let mut port_conf_file = path.join(PORT_CONF_DIR);

    if create_dirs {
        fs_err!(fs::create_dir_all, &path, "create firewall config dir")?;
        fs_err!(
            fs::create_dir_all,
            &net_conf_file,
            "create network config dir"
        )?;
        fs_err!(
            fs::create_dir_all,
            &port_conf_file,
            "create port config dir"
        )?;
    }
    if !network_id.is_empty() && !container_id.is_empty() {
        net_conf_file.push(network_id);
        port_conf_file.push(network_id.to_string() + "_" + container_id);
    }

    Ok(FilePaths {
        fw_driver_file,
        net_conf_file,
        port_conf_file,
    })
}

/// Store the firewall configs on disk.
/// This should be caller after firewall setup to allow the firewalld reload
/// service to read the configs later and readd the rules.
pub fn write_fw_config(
    config_dir: &str,
    network_id: &str,
    container_id: &str,
    fw_driver: &str,
    net_conf: &SetupNetwork,
    port_conf: &PortForwardConfig,
) -> NetavarkResult<()> {
    let paths = get_file_paths(config_dir, network_id, container_id, true)?;
    fs_err!(
        File::create,
        &paths.fw_driver_file,
        "create firewall-driver file"
    )?
    .write_all(fw_driver.as_bytes())
    .map_err(|err| NetavarkError::wrap("failed to write firewall-driver file", err.into()))?;

    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&paths.net_conf_file)
    {
        Ok(f) => serde_json::to_writer(f, &net_conf)?,
        // net config file already exists no need to write the same stuff again.
        Err(ref e) if e.kind() == ErrorKind::AlreadyExists => (),
        Err(e) => {
            return Err(NetavarkError::wrap(
                format!("create network config {:?}", &paths.net_conf_file.display()),
                e.into(),
            ));
        }
    };

    let ports_file = fs_err!(File::create, &paths.port_conf_file, "create port config")?;
    serde_json::to_writer(ports_file, &port_conf)?;

    Ok(())
}

/// Remove firewall config files.
/// On firewall teardown remove the specific config files again so the
/// firewalld reload service does not keep using them.
pub fn remove_fw_config(
    config_dir: &str,
    network_id: &str,
    container_id: &str,
    complete_teardown: bool,
) -> NetavarkResult<()> {
    let paths = get_file_paths(config_dir, network_id, container_id, false)?;
    fs_err!(
        remove_file_ignore_enoent,
        &paths.port_conf_file,
        "remove port config"
    )?;
    if complete_teardown {
        fs_err!(
            remove_file_ignore_enoent,
            &paths.net_conf_file,
            "remove network config"
        )?;
    }
    Ok(())
}

pub struct FirewallConfig {
    /// Name of the firewall driver
    pub driver: String,
    /// All the network firewall configs
    pub net_confs: Vec<SetupNetwork>,
    /// All port forwarding configs
    pub port_confs: Vec<PortForwardConfigOwned>,
}

/// Read all firewall configs files from the dir.
pub fn read_fw_config(config_dir: &str) -> NetavarkResult<FirewallConfig> {
    let paths = get_file_paths(config_dir, "", "", false)?;

    let driver = fs_err!(
        fs::read_to_string,
        &paths.fw_driver_file,
        "read firewall-driver"
    )?;

    let net_confs = read_dir_conf(paths.net_conf_file)?;
    let port_confs = read_dir_conf(paths.port_conf_file)?;

    Ok(FirewallConfig {
        driver,
        net_confs,
        port_confs,
    })
}

fn read_dir_conf<T: DeserializeOwned>(dir: PathBuf) -> NetavarkResult<Vec<T>> {
    let mut confs = Vec::new();
    for entry in fs_err!(fs::read_dir, &dir, "read dir")? {
        let entry = entry?;
        let content = fs_err!(fs::read_to_string, entry.path(), "read config")?;
        // Note one might think we should use from_reader() instated of reading
        // into one string. However the files we act on are small enough that it
        // should't matter to have the content into memory at once and based on
        // https://github.com/serde-rs/json/issues/160 this here is much faster.
        let conf: T = serde_json::from_str(&content)?;
        confs.push(conf);
    }
    Ok(confs)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use crate::network::internal_types::IsolateOption;

    use super::*;
    use tempfile::Builder;

    #[test]
    fn test_fw_config() {
        let network_id = "abc";
        let container_id = "123";
        let driver = "iptables";

        let tmpdir = Builder::new().prefix("netavark-tests").tempdir().unwrap();
        let config_dir = tmpdir.path().to_str().unwrap();

        let net_conf = SetupNetwork {
            subnets: Some(vec!["10.0.0.0/24".parse().unwrap()]),
            bridge_name: "bridge".to_string(),
            network_hash_name: "hash".to_string(),
            isolation: IsolateOption::Never,
            dns_port: 53,
        };
        let net_conf_json = r#"{"subnets":["10.0.0.0/24"],"bridge_name":"bridge","network_hash_name":"hash","isolation":"Never","dns_port":53}"#;

        let port_conf = PortForwardConfig {
            container_id: container_id.to_string(),
            port_mappings: &None,
            network_name: "name".to_string(),
            network_hash_name: "hash".to_string(),
            container_ip_v4: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))),
            subnet_v4: Some("10.0.0.0/24".parse().unwrap()),
            container_ip_v6: None,
            subnet_v6: None,
            dns_port: 53,
            dns_server_ips: &vec![],
        };
        let port_conf_json = r#"{"container_id":"123","port_mappings":null,"network_name":"name","network_hash_name":"hash","container_ip_v4":"10.0.0.2","subnet_v4":"10.0.0.0/24","container_ip_v6":null,"subnet_v6":null,"dns_port":53,"dns_server_ips":[]}"#;

        let res = write_fw_config(
            config_dir,
            network_id,
            container_id,
            driver,
            &net_conf,
            &port_conf,
        );

        assert!(res.is_ok(), "write_fw_config failed");

        let paths = get_file_paths(config_dir, network_id, container_id, false).unwrap();

        let res = fs::read_to_string(paths.fw_driver_file).unwrap();
        assert_eq!(res, "iptables", "read fw driver");

        let res = fs::read_to_string(&paths.net_conf_file).unwrap();
        assert_eq!(res, net_conf_json, "read net conf");

        let res = fs::read_to_string(&paths.port_conf_file).unwrap();
        assert_eq!(res, port_conf_json, "read port conf");

        let res = read_fw_config(config_dir).unwrap();
        assert_eq!(res.driver, driver, "correct fw driver");
        assert_eq!(res.net_confs, vec![net_conf], "same net configs");
        let port_confs_ref: Vec<PortForwardConfig> =
            res.port_confs.iter().map(|f| f.into()).collect();
        assert_eq!(port_confs_ref, vec![port_conf], "same port configs");

        let res = remove_fw_config(config_dir, network_id, container_id, true);
        assert!(res.is_ok(), "remove_fw_config failed");

        assert_eq!(
            paths.net_conf_file.exists(),
            false,
            "net conf should not exists"
        );
        assert_eq!(
            paths.port_conf_file.exists(),
            false,
            "port conf should not exists"
        );

        // now again since we ignore ENOENT it should still return no error
        let res = remove_fw_config(config_dir, network_id, container_id, true);
        assert!(res.is_ok(), "remove_fw_config failed second time");
    }
}
