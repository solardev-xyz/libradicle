//! `radicle-node` as libradicle builds it: with `i2p`, so that an I2P address
//! a peer relays decodes rather than failing (which would disconnect that
//! peer), and with I2P dialling dropped unless `node.i2p` routes it.

use radicle::node::config::{AddressConfig, Config};
use radicle::node::{Address, Alias, HostName};

#[test]
fn an_i2p_address_decodes_and_is_not_dialled_by_default() {
    let address: Address = "f2atcc7udeub5kh4nkljtjwyk7ikjviorufzgwnfwhkphljl3vhq.b32.i2p:8776"
        .parse()
        .expect("an I2P address");
    assert!(matches!(address.host(), HostName::I2p(_)));
    assert_eq!(Config::new(Alias::new("features")).i2p, AddressConfig::Drop);
}
