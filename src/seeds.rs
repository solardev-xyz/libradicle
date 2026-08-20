use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;

use radicle::node::address::{KnownAddress, Source, Store as _};
use radicle::node::config::ConnectAddress;
use radicle::node::{Address, Alias, Features, NodeId, UserAgent};
use radicle::profile::Profile;

use crate::Error;

pub(crate) const DIAL_CONCURRENCY: usize = 6;
pub(crate) const READY_TARGET: usize = 4;

const IRIS: &str = "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7";
const ROSA: &str = "z6Mkmqogy2qEM2ummccUthFEaaHvyYmYBYh3dbe9W4ebScxo";

/// Cold-start prior derived from the dedicated, DNS-named seed tier in the
/// 2026-08-20 indexer snapshot. Volatile health metrics deliberately do not
/// live here; `node.db` supplies device-local history after the first start.
const SHIPPED: &[(&str, &str)] = &[
    (
        "radicle-seed.b4mad.industries",
        "z6MktPgLsgsww5zavvM4UownrwhFK2AkwZLjnzDPbhngbJWC@radicle-seed.b4mad.industries:8776",
    ),
    (
        "radicle.pastilhas.dev",
        "z6MkgcsJQsnEZy66CbJh8LpCL31JEmooNrKjTNR5J4dzx57K@radicle.pastilhas.dev:8776",
    ),
    (
        "radicle.at",
        "z6MkjDYUKMUeY58Vtr8dGJrHRvnTfjKWVGCBYJDVTHXsXzm5@seed.radicle.at:8776",
    ),
    (
        "radicle.spacetime.technology",
        "z6Mkq9eUruFTBtgqqLAHGR8gmsnC556ftMQsNuNravqYL4Dw@radicle.spacetime.technology:8776",
    ),
    (
        "seed.cielago.xyz",
        "z6MkmvNrdsAihAGD4vz3wN7U8hy7SmZpBoDe28K1m3GFjkf2@seed.cielago.xyz:8776",
    ),
    (
        "seed.kat5.dev",
        "z6Mkif1kaohyuJFSZ43d4V6HFJWwQo7rvLFHbkanRYSt9EL5@seed.kat5.dev:8776",
    ),
    (
        "seed.pipapo.org",
        "z6MkmXdBVkNiieUuEAgS6Td4qMzqP969tFfTpe3Lx79iS2Wf@seed.pipapo.org:8776",
    ),
    (
        "seed.moist.place",
        "z6MkveTP2pCvbQ2VRexSdUG7kmEXWkuFYH5Aj1hKMwD3Gcx4@seed.moist.place:8776",
    ),
    (
        "seed.petal.cafe",
        "z6MkhHXZVv2g2Edg9WNBDrAY8r6VTvCeqX3q8BpSfxfptYL9@seed.petal.cafe:8776",
    ),
    (
        "rosa.radicle.network",
        "z6Mkmqogy2qEM2ummccUthFEaaHvyYmYBYh3dbe9W4ebScxo@rosa.radicle.network:58776",
    ),
    (
        "iris.radicle.network",
        "z6MkrLMMsiPWUcNPHcRajuMi9mDfYckSoJyPwwnknocNYPm7@iris.radicle.network:58776",
    ),
    (
        "seed.heptapod.dev",
        "z6MkiwJ4Y9Zuwx8oeTftuqQNdBDd9g6M8XCUoQXPz6bgkbQ7@seed.heptapod.dev:8776",
    ),
    (
        "radicle.jarg.io",
        "z6MkwfrBy9mKTfcVELcV4wc6zfN379FPMnAqsxnwt4j2TdQ2@radicle.jarg.io:8776",
    ),
    (
        "seed.thefarshore.dev",
        "z6MkvswafXNZe3VeZG8yAuxbXiwMV4paK19nZQLptxs6r7pE@seed.thefarshore.dev:8776",
    ),
];

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub alias: String,
    pub address: ConnectAddress,
}

impl Candidate {
    pub fn nid(&self) -> NodeId {
        self.address.id
    }
}

fn shipped() -> Result<Vec<Candidate>, Error> {
    SHIPPED
        .iter()
        .map(|(alias, address)| {
            let (nid, addr) = address.split_once('@').ok_or_else(|| {
                Error::NodeThread(format!("invalid shipped seed {address}: missing @"))
            })?;
            let nid = NodeId::from_str(nid).map_err(|err| {
                Error::NodeThread(format!("invalid shipped seed node ID {nid}: {err}"))
            })?;
            let addr = Address::from_str(addr).map_err(|err| {
                Error::NodeThread(format!("invalid shipped seed address {addr}: {err}"))
            })?;
            let address = ConnectAddress::from((nid, addr));
            Ok(Candidate {
                alias: (*alias).to_owned(),
                address,
            })
        })
        .collect()
}

fn successful_history(profile: &Profile) -> BTreeMap<NodeId, u64> {
    let mut history = BTreeMap::new();
    let database = profile.config.node.database.clone();
    let Ok(store) = profile.home.addresses(database) else {
        return history;
    };
    let Ok(entries) = store.entries() else {
        return history;
    };
    for entry in entries {
        let Some(success) = entry.address.last_success else {
            continue;
        };
        if entry
            .address
            .last_attempt
            .is_some_and(|attempt| attempt > success)
        {
            continue;
        }
        history
            .entry(entry.node)
            .and_modify(|latest| *latest = (*latest).max(success.as_millis()))
            .or_insert_with(|| success.as_millis());
    }
    history
}

/// Merge explicit user preferences with the device's successful seed history
/// and finally the shipped cold-start prior. The historical Iris/Rosa defaults
/// are recognized by node ID and therefore do not jump to the front merely
/// because Heartwood wrote them into a new profile.
pub(crate) fn effective(profile: &Profile) -> Result<Vec<Candidate>, Error> {
    // An explicitly empty preferred-seed list is the established way for a
    // host to request an isolated node. Do not silently defeat that intent
    // with shipped defaults.
    if profile.config.preferred_seeds.is_empty() {
        return Ok(Vec::new());
    }
    let mut shipped = shipped()?;
    let history = successful_history(profile);
    shipped.sort_by(|left, right| {
        history
            .get(&right.nid())
            .cmp(&history.get(&left.nid()))
            .then_with(|| {
                let left_rank = SHIPPED
                    .iter()
                    .position(|(_, address)| address.starts_with(&left.nid().to_string()))
                    .unwrap_or(usize::MAX);
                let right_rank = SHIPPED
                    .iter()
                    .position(|(_, address)| address.starts_with(&right.nid().to_string()))
                    .unwrap_or(usize::MAX);
                left_rank.cmp(&right_rank)
            })
    });

    let iris = NodeId::from_str(IRIS)
        .map_err(|err| Error::NodeThread(format!("invalid Iris node ID: {err}")))?;
    let rosa = NodeId::from_str(ROSA)
        .map_err(|err| Error::NodeThread(format!("invalid Rosa node ID: {err}")))?;
    let mut result = Vec::new();
    let mut seen = HashSet::new();

    for address in &profile.config.preferred_seeds {
        if address.id == iris || address.id == rosa || !seen.insert(address.id) {
            continue;
        }
        result.push(Candidate {
            alias: "configured-seed".to_owned(),
            address: address.clone(),
        });
    }
    for candidate in shipped {
        if seen.insert(candidate.nid()) {
            result.push(candidate);
        }
    }
    Ok(result)
}

/// Ensure every shipped seed exists in Heartwood's address book. This runs
/// after `Runtime::init` has created the database but before `Runtime::run`
/// starts, so the insert cannot race the node thread.
pub(crate) fn install(profile: &Profile) -> Result<(), Error> {
    let database = profile.config.node.database.clone();
    let mut store = profile
        .home
        .addresses(database)
        .map_err(|err| Error::NodeThread(format!("open seed address book: {err}")))?;
    let timestamp = radicle::profile::env::local_time().into();
    let bootstrap_agent = UserAgent::from_str("/libradicle/bootstrap/")
        .map_err(|err| Error::NodeThread(format!("invalid bootstrap agent: {err}")))?;

    for candidate in shipped()? {
        let nid = candidate.nid();
        let address = candidate.address.addr.clone();
        let existing = store
            .get(&nid)
            .map_err(|err| Error::NodeThread(format!("read seed {nid}: {err}")))?;
        if existing
            .as_ref()
            .is_some_and(|node| node.addrs.iter().any(|known| known.addr == address))
        {
            continue;
        }

        let (version, features, alias, pow, agent, published) = match existing {
            Some(node) => (
                node.version,
                node.features,
                node.alias,
                node.pow,
                node.agent,
                node.timestamp,
            ),
            None => (
                1,
                Features::SEED,
                Alias::new(&candidate.alias),
                0,
                bootstrap_agent.clone(),
                timestamp,
            ),
        };
        store
            .insert(
                &nid,
                version,
                features,
                &alias,
                pow,
                &agent,
                published,
                [KnownAddress::new(address, Source::Bootstrap)],
            )
            .map_err(|err| Error::NodeThread(format!("install seed {nid}: {err}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{shipped, IRIS, ROSA, SHIPPED};

    #[test]
    fn shipped_book_is_valid_unique_and_dns_only() {
        let seeds = shipped().expect("parse shipped seed book");
        assert_eq!(seeds.len(), 14);
        assert_eq!(
            seeds
                .iter()
                .map(|seed| seed.nid())
                .collect::<HashSet<_>>()
                .len(),
            14
        );
        for seed in seeds {
            let rendered = seed.address.to_string();
            assert!(
                !rendered.contains(".onion"),
                "onion seed shipped: {rendered}"
            );
            assert!(rendered.contains('@') && rendered.contains('.'));
        }
    }

    #[test]
    fn operator_seeds_are_demoted_in_the_cold_start_prior() {
        let iris = SHIPPED
            .iter()
            .position(|(_, address)| address.starts_with(IRIS))
            .expect("Iris is present");
        let rosa = SHIPPED
            .iter()
            .position(|(_, address)| address.starts_with(ROSA))
            .expect("Rosa is present");
        assert!(iris >= 8);
        assert!(rosa >= 8);
    }
}
