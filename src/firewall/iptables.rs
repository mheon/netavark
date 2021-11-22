use crate::firewall;
use crate::network::core_utils::CoreUtils;
use crate::network::types;
use crate::network::types::{Network, PerNetworkOptions, TeardownPortForward};
use iptables;
use iptables::IPTables;
use log::debug;
use std::error::Error;

const HEXMARK: &str = "0x2000";
pub(crate) const MAX_HASH_SIZE: usize = 13;

//  CHAIN NAMES
const NAT: &str = "nat";
const PRIV_CHAIN_NAME: &str = "NETAVARK_FORWARD";
const HOSTPORT_DNAT_CHAIN: &str = "NETAVARK-HOSTPORT-DNAT";
const HOSTPORT_SETMARK_CHAIN: &str = "NETAVARK-HOSTPORT-SETMARK";
const NETAVARK_HOSTPORT_MASK_CHAIN: &str = "NETAVARK-HOSTPORT-MASQ";
const CONTAINER_DN_CHAIN: &str = "NETAVARK-DN-";
const CONTAINER_CHAIN: &str = "NETAVARK-";
const PREROUTING_CHAIN: &str = "PREROUTING";
const OUTPUT_CHAIN: &str = "OUTPUT";

// JUMP DEST
const POSTROUTING_JUMP: &str = "POSTROUTING";
const FILTER_JUMP: &str = "filter";
const MARK_JUMP: &str = "MARK";
const DNAT_JUMP: &str = "DNAT";
const MASQ_JUMP: &str = "MASQUERADE";
const ACCEPT_JUMP: &str = "ACCEPT";

// Iptables driver - uses direct iptables commands via the iptables crate.
pub struct IptablesDriver {
    conn: IPTables,
}

pub fn new() -> Result<Box<dyn firewall::FirewallDriver>, Box<dyn Error>> {
    // create an iptables connection
    let ipt = iptables::new(false)?;
    let driver = IptablesDriver { conn: ipt };
    Ok(Box::new(driver))
}

impl firewall::FirewallDriver for IptablesDriver {
    fn setup_network(
        &self,
        net: types::Network,
        network_hash_name: String,
    ) -> Result<(), Box<dyn Error>> {
        if let Some(subnet) = net.subnets {
            for network in subnet {
                let prefixed_network_hash_name = format!("{}-{}", "NETAVARK", network_hash_name);
                add_chain_unique(&self.conn, NAT, &prefixed_network_hash_name)?;

                // declare the rule
                let nat_rule =
                    format!("-d {} -j {}", network.subnet.to_string(), ACCEPT_JUMP).to_string();
                append_unique(&self.conn, NAT, &prefixed_network_hash_name, &nat_rule)?;

                //  Add first rule for the network
                let masq_rule = "! -d 224.0.0.0/4 -j MASQUERADE".to_string();
                append_unique(&self.conn, NAT, &prefixed_network_hash_name, &masq_rule)?;

                //  Add private chain name if it does not exist
                add_chain_unique(&self.conn, FILTER_JUMP, PRIV_CHAIN_NAME)?;

                //  Create netavark firewall rule
                let netavark_fw = format!(
                    "-m comment --comment 'netavark firewall plugin rules' -j {}",
                    PRIV_CHAIN_NAME
                );
                // Insert the rule into the first position
                if !self.conn.exists(FILTER_JUMP, "FORWARD", &netavark_fw)? {
                    self.conn
                        .insert(FILTER_JUMP, "FORWARD", &netavark_fw, 1)
                        .map(|_| debug_rule_create(FILTER_JUMP, "FORWARD", netavark_fw))?;
                }
                // Create incoming traffic rule
                // CNI did this by IP address, this is implemented per subnet
                let allow_incoming_rule = format!(
                    "-d {} -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT",
                    network.subnet.to_string()
                );

                append_unique(
                    &self.conn,
                    FILTER_JUMP,
                    PRIV_CHAIN_NAME,
                    &allow_incoming_rule,
                )?;

                // Create outgoing traffic rule
                // CNI did this by IP address, this is implemented per subnet
                let allow_outgoing_rule = format!("-s {} -j ACCEPT", network.subnet.to_string());
                append_unique(
                    &self.conn,
                    FILTER_JUMP,
                    PRIV_CHAIN_NAME,
                    &allow_outgoing_rule,
                )?;
            }
        }
        Ok(())
    }
    // teardown_network should only be called in the case of
    // a complete teardown.
    fn teardown_network(
        &self,
        net: types::Network,
        complete_teardown: bool,
    ) -> Result<(), Box<dyn Error>> {
        // Remove network specific general NAT rules
        if let Some(subnet) = net.subnets {
            for network in subnet {
                let allow_incoming_rule = format!(
                    "-d {} -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT",
                    network.subnet.to_string()
                );

                append_unique(
                    &self.conn,
                    FILTER_JUMP,
                    PRIV_CHAIN_NAME,
                    &allow_incoming_rule,
                )?;

                // Create outgoing traffic rule
                // CNI did this by IP address, this is implemented per subnet
                let allow_outgoing_rule = format!("-s {} -j ACCEPT", network.subnet.to_string());
                append_unique(
                    &self.conn,
                    FILTER_JUMP,
                    PRIV_CHAIN_NAME,
                    &allow_outgoing_rule,
                )?;
                if complete_teardown {
                    let allow_incoming_rule = format!(
                        "-d {} -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT",
                        network.subnet.to_string()
                    );

                    remove_if_rule_exists(
                        &self.conn,
                        FILTER_JUMP,
                        PRIV_CHAIN_NAME,
                        &allow_incoming_rule,
                    )?;

                    // CNI did this by IP address, this is implemented per subnet
                    let allow_outgoing_rule =
                        format!("-s {} -j ACCEPT", network.subnet.to_string());
                    remove_if_rule_exists(
                        &self.conn,
                        FILTER_JUMP,
                        PRIV_CHAIN_NAME,
                        &allow_outgoing_rule,
                    )?;
                }
            }
        }
        Result::Ok(())
    }

    fn setup_port_forward(
        &self,
        network: Network,
        container_id: &str,
        port_mappings: Vec<types::PortMapping>,
        network_name: &str,
        id_network_hash: &str,
        options: &PerNetworkOptions,
    ) -> Result<(), Box<dyn Error>> {
        // Need to enable sysctl localnet so that traffic can pass
        // through localhost to containers
        let network_interface = network.network_interface;
        match network_interface {
            None => {}
            Some(i) => {
                let localnet_path = format!("net.ipv4.conf.{}.route_localnet", i);
                CoreUtils::apply_sysctl_value(localnet_path.as_str(), "1")?;
            }
        }
        let container_ips = options.static_ips.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "no container ip provided")
        })?;
        let container_ip = container_ips[0];
        let networks = &network.subnets.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "no network address provided")
        })?;
        let container_network_address = networks[0].subnet;
        // Set up all chains
        let network_dn_chain_name = CONTAINER_DN_CHAIN.to_owned() + id_network_hash;
        let network_chain_name = CONTAINER_CHAIN.to_owned() + id_network_hash;

        let comment_network_cid = format!(
            "-m comment --comment 'name: {} id: {}'",
            network_name, container_id
        );
        let comment_dn_network_cid = format!(
            "-m comment --comment 'dnat name: {} id: {}'",
            network_name, container_id
        );
        // Make sure chains exist or create them
        add_chain_unique(&self.conn, NAT, HOSTPORT_DNAT_CHAIN)?;
        add_chain_unique(&self.conn, NAT, HOSTPORT_SETMARK_CHAIN)?;
        add_chain_unique(&self.conn, NAT, NETAVARK_HOSTPORT_MASK_CHAIN)?;
        add_chain_unique(&self.conn, NAT, &network_dn_chain_name)?;

        // Setup one-off rules that have nothing to do with ports
        // PREROUTING
        let prerouting_rule = format!("-j {} -m addrtype --dst-type LOCAL", HOSTPORT_DNAT_CHAIN);
        append_unique(&self.conn, NAT, PREROUTING_CHAIN, &prerouting_rule)?;

        // OUTPUT
        let portmap_output_rule =
            format!("-j {} -m addrtype --dst-type LOCAL", HOSTPORT_DNAT_CHAIN);
        append_unique(&self.conn, NAT, OUTPUT_CHAIN, &portmap_output_rule)?;

        //  SETMARK-CHAIN
        let setmark_rule = format!("-j {}  --set-xmark {}/{}", MARK_JUMP, HEXMARK, HEXMARK);
        append_unique(&self.conn, NAT, HOSTPORT_SETMARK_CHAIN, &setmark_rule)?;

        //  HOSTPORT-MASQ
        let hostport_masq_rule = format!(
            "-j {} -m comment --comment 'netavark portfw masq mark' -m mark --mark {}/{}",
            MASQ_JUMP, HEXMARK, HEXMARK
        );
        append_unique(
            &self.conn,
            NAT,
            NETAVARK_HOSTPORT_MASK_CHAIN,
            &hostport_masq_rule,
        )?;

        // POSTROUTING
        append_unique(
            &self.conn,
            NAT,
            POSTROUTING_JUMP,
            &format!("-j {} ", NETAVARK_HOSTPORT_MASK_CHAIN),
        )?;

        append_unique(
            &self.conn,
            NAT,
            POSTROUTING_JUMP,
            &format!(
                "-j {} -s {} {}",
                network_chain_name,
                container_ip.to_string(),
                comment_network_cid
            ),
        )?;

        // FOR EACH PORT
        for i in port_mappings {
            // hostport dnat
            let hostport_dnat_rule = format!(
                "-j {} -p tcp -m multiport --destination-ports {} {}",
                network_dn_chain_name,
                i.host_port.to_string(),
                comment_dn_network_cid
            );
            append_unique(&self.conn, NAT, HOSTPORT_DNAT_CHAIN, &hostport_dnat_rule)?;
            // dn container (the actual port usages)
            let setmark_network_rule = format!(
                "-j {} -s {} -p tcp --dport {}",
                HOSTPORT_SETMARK_CHAIN,
                container_network_address.to_string(),
                i.host_port.to_string()
            );
            append_unique(
                &self.conn,
                NAT,
                &network_dn_chain_name,
                &setmark_network_rule,
            )?;
            let setmark_localhost_rule = format!(
                "-j {} -s 127.0.0.1 -p tcp --dport {}",
                HOSTPORT_SETMARK_CHAIN,
                i.host_port.to_string()
            );
            append_unique(
                &self.conn,
                NAT,
                &network_dn_chain_name,
                &setmark_localhost_rule,
            )?;
            let container_dest_rule = format!(
                "-j {} -p tcp --to-destination {}:{} --destination-port {}",
                DNAT_JUMP,
                container_ip.to_string(),
                i.container_port.to_string(),
                i.host_port.to_string()
            );
            append_unique(
                &self.conn,
                NAT,
                &network_dn_chain_name,
                &container_dest_rule,
            )?;
        }

        Result::Ok(())
    }

    fn teardown_port_forward(&self, tear: TeardownPortForward) -> Result<(), Box<dyn Error>> {
        let container_ips = tear.options.static_ips.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "no container ip provided")
        })?;
        let networks = tear.network.subnets.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "no network address provided")
        })?;
        let container_network_address = networks[0].subnet;
        let network_dn_chain_name = CONTAINER_DN_CHAIN.to_owned() + tear.id_network_hash.as_ref();
        let comment_dn_network_cid = format!(
            "-m comment --comment 'dnat name: {} id: {}'",
            tear.network_name, tear.container_id
        );
        let network_chain_name = CONTAINER_CHAIN.to_owned() + tear.id_network_hash.as_ref();
        let container_ip = container_ips[0];
        // First delete any container specific rules
        // POSTROUTING
        let comment_network_cid = format!(
            "-m comment --comment 'name: {} id: {}'",
            tear.network_name, tear.container_id
        );
        remove_if_rule_exists(
            &self.conn,
            NAT,
            POSTROUTING_JUMP,
            &format!(
                "-j {} -s {} {}",
                network_chain_name,
                container_ip.to_string(),
                comment_network_cid
            ),
        )?;

        // Iterate on ports
        for i in tear.port_mappings {
            // hostport dnat
            let hostport_dnat_rule = format!(
                "-j {} -p tcp -m multiport --destination-ports {} {}",
                network_dn_chain_name,
                i.host_port.to_string(),
                comment_dn_network_cid
            );
            remove_if_rule_exists(&self.conn, NAT, HOSTPORT_DNAT_CHAIN, &hostport_dnat_rule)?;
            // dn container (the actual port usages)
            let setmark_network_rule = format!(
                "-j {} -s {} -p tcp --dport {}",
                HOSTPORT_SETMARK_CHAIN,
                container_network_address.to_string(),
                i.host_port.to_string()
            );
            remove_if_rule_exists(
                &self.conn,
                NAT,
                &network_dn_chain_name,
                &setmark_network_rule,
            )?;
            let setmark_localhost_rule = format!(
                "-j {} -s 127.0.0.1 -p tcp --dport {}",
                HOSTPORT_SETMARK_CHAIN,
                i.host_port.to_string()
            );
            remove_if_rule_exists(
                &self.conn,
                NAT,
                &network_dn_chain_name,
                &setmark_localhost_rule,
            )?;
            let container_dest_rule = format!(
                "-j {} -p tcp --to-destination {}:{} --destination-port {}",
                DNAT_JUMP,
                container_ip.to_string(),
                i.container_port.to_string(),
                i.host_port.to_string()
            );
            remove_if_rule_exists(
                &self.conn,
                NAT,
                &network_dn_chain_name,
                &container_dest_rule,
            )?;
        }
        // If last container on the network, then teardown network based rules
        if tear.complete_teardown {
            // Remove the entire NETAVARK-<HASH> chain
            remove_chain_and_rules(&self.conn, NAT, &network_chain_name)?;
            // Remove the entire NETAVARK-DN-<HASH> chain
            remove_chain_and_rules(&self.conn, NAT, &network_dn_chain_name)?;
        }
        Result::Ok(())
    }
}
// append a rule to chain if it does not exist
// Note: While there is an API provided for this exact thing, the API returns
// an error that is not defined if the rule exists.  This function just returns
// an error if there is a problem.
fn append_unique(
    driver: &IPTables,
    table: &str,
    chain: &str,
    rule: &str,
) -> Result<(), Box<dyn Error>> {
    let exists = driver.exists(table, chain, rule)?;
    if exists {
        return Ok(());
    }
    debug_rule_exists(table, chain, rule.to_string());
    driver
        .append(table, chain, rule)
        .map(|_| debug_rule_create(table, chain, rule.to_string()))
}

// add a chain if it does not exist, else do nothing
fn add_chain_unique(driver: &IPTables, table: &str, new_chain: &str) -> Result<(), Box<dyn Error>> {
    // Note: while there is an API provided to check if a chain exists in a table
    // by iptables, it, for some reason, is slow.  Instead we just get a list of
    // chains in a table and iterate.  Same is being done in golang implementations
    let exists = chain_exists(driver, table, new_chain)?;
    if exists {
        debug_chain_exists(table, new_chain);
        return Ok(());
    }
    driver
        .new_chain(table, new_chain)
        .map(|_| debug_chain_create(table, new_chain))
}

// returns a bool as to whether the chain exists
fn chain_exists(driver: &IPTables, table: &str, chain: &str) -> Result<bool, Box<dyn Error>> {
    let c = driver.list_chains(table)?;
    if c.iter().any(|i| i == chain) {
        debug_chain_exists(table, chain);
        return serde::__private::Result::Ok(true);
    }
    serde::__private::Result::Ok(false)
}

fn remove_chain_and_rules(
    driver: &IPTables,
    table: &str,
    chain: &str,
) -> Result<(), Box<dyn Error>> {
    let exists = chain_exists(driver, table, chain)?;
    // If the chain is not there, we cannot delete the rules.  This
    // should not be fatal
    if !exists {
        return Result::Ok(());
    }
    driver.flush_chain(table, chain)?;
    driver.delete_chain(table, chain)
}

fn remove_if_rule_exists(
    driver: &IPTables,
    table: &str,
    chain: &str,
    rule: &str,
) -> Result<(), Box<dyn Error>> {
    // If the rule is not present, do not error
    let exists = driver.exists(table, chain, rule)?;
    if !exists {
        debug_rule_no_exists(table, chain, rule.to_string());
        return Ok(());
    }
    driver.delete(table, chain, rule)
}

fn debug_chain_create(table: &str, chain: &str) {
    debug!("chain {} created on table {}", chain, table);
}

fn debug_chain_exists(table: &str, chain: &str) {
    debug!("chain {} exists on table {}", chain, table);
}

fn debug_rule_create(table: &str, chain: &str, rule: String) {
    debug!(
        "rule {} created on table {} and chain {}",
        rule, table, chain
    );
}

fn debug_rule_exists(table: &str, chain: &str, rule: String) {
    debug!(
        "rule {} exists on table {} and chain {}",
        rule, table, chain
    );
}
fn debug_rule_no_exists(table: &str, chain: &str, rule: String) {
    debug!(
        "no rule {} exists on table {} and chain {}",
        rule, table, chain
    );
}
