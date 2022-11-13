use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, format_err};
use fedimint_api::cancellable::{Cancellable, Cancelled};
use fedimint_api::config::{
    BitcoindRpcCfg, ClientConfig, DkgPeerMsg, DkgRunner, ModuleConfigGenParams, Node,
    ServerModuleConfig, TypedServerModuleConfig,
};
use fedimint_api::module::FederationModuleConfigGen;
use fedimint_api::multiplexed::ModuleMultiplexer;
use fedimint_api::net::peers::AnyPeerConnections;
use fedimint_api::task::TaskGroup;
use fedimint_api::{Amount, PeerId};
pub use fedimint_core::config::*;
use fedimint_core::modules::ln::config::LightningModuleConfig;
use fedimint_core::modules::ln::LightningModuleConfigGen;
use fedimint_core::modules::mint::config::MintConfig;
use fedimint_core::modules::mint::MintConfigGenerator;
use fedimint_wallet::config::WalletConfig;
use fedimint_wallet::WalletConfigGenerator;
use hbbft::crypto::serde_impl::SerdeSecret;
use rand::{CryptoRng, RngCore};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio_rustls::rustls;
use tracing::info;
use url::Url;

use crate::fedimint_api::net::peers::PeerConnections;
use crate::fedimint_api::NumPeers;
use crate::net::connect::Connector;
use crate::net::connect::TlsConfig;
use crate::net::peers::{ConnectionConfig, NetworkConfig};
use crate::{ReconnectPeerConnections, TlsTcpConnector};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub federation_name: String,
    pub identity: PeerId,
    pub hbbft_bind_addr: String,
    pub api_bind_addr: String,
    #[serde(with = "serde_tls_cert")]
    pub tls_cert: rustls::Certificate,
    #[serde(with = "serde_tls_key")]
    pub tls_key: rustls::PrivateKey,

    pub peers: BTreeMap<PeerId, Peer>,
    #[serde(with = "serde_binary_human_readable")]
    pub hbbft_sks: SerdeSecret<hbbft::crypto::SecretKeyShare>,
    #[serde(with = "serde_binary_human_readable")]
    pub hbbft_pk_set: hbbft::crypto::PublicKeySet,

    #[serde(with = "serde_binary_human_readable")]
    pub epoch_sks: SerdeSecret<hbbft::crypto::SecretKeyShare>,
    #[serde(with = "serde_binary_human_readable")]
    pub epoch_pk_set: hbbft::crypto::PublicKeySet,

    pub modules: BTreeMap<String, ServerModuleConfig>,
}

impl ServerConfig {
    pub fn get_module_config<T: TypedServerModuleConfig>(&self, name: &str) -> anyhow::Result<T> {
        self.modules
            .get(name)
            .ok_or_else(|| format_err!("Module {name} not found"))?
            .to_typed()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub hbbft: ConnectionConfig,
    #[serde(with = "serde_tls_cert")]
    pub tls_cert: rustls::Certificate,
    /// The peer's websocket network address and port (e.g. `ws://10.42.0.10:5000`)
    pub api_addr: Url,
    /// human-readable name
    pub name: String,
}

pub struct FederationeConfigGenParams {
    pub mint_amounts: Vec<Amount>,
    pub bitcoin_rpc: BitcoindRpcCfg,

    /// extra options for extra settings and modules
    pub other: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
/// network config for a server
pub struct ServerConfigParams {
    pub tls: TlsConfig,
    pub hbbft: NetworkConfig,
    pub api: NetworkConfig,
    pub server_dkg: NetworkConfig,
    // TODO: delete unused dkgs
    pub amount_tiers: Vec<Amount>,
    pub federation_name: String,
    pub bitcoind_rpc: String,

    /// extra options for extra settings and modules
    pub other: BTreeMap<String, serde_json::Value>,
}

impl ServerConfig {
    pub fn to_client_config(&self) -> ClientConfig {
        let nodes = self
            .peers
            .iter()
            .map(|(peer_id, peer)| Node {
                url: peer.api_addr.clone(),
                name: format!("node #{}", peer_id),
            })
            .collect();

        ClientConfig {
            federation_name: self.federation_name.clone(),
            nodes,
            modules: self
                .modules
                .iter()
                .map(|(name, cfg)| {
                    (
                        name.clone(),
                        match name.as_str() {
                            "wallet" => cfg
                                .to_typed::<WalletConfig>()
                                .expect("valid config should not fail")
                                .to_client_config(),
                            "mint" => cfg
                                .to_typed::<MintConfig>()
                                .expect("valid config should not fail")
                                .to_client_config(),
                            "ln" => cfg
                                .to_typed::<LightningModuleConfig>()
                                .expect("valid config should not fail")
                                .to_client_config(),
                            other => panic!("module name that was not expected: {other}"),
                        },
                    )
                })
                .collect(),
        }
    }

    pub fn validate_config(&self, identity: &PeerId) -> anyhow::Result<()> {
        if self.epoch_sks.public_key_share()
            != self.epoch_pk_set.public_key_share(identity.to_usize())
        {
            bail!("Epoch private key doesn't match pubkey share");
        }
        if self.hbbft_sks.public_key_share()
            != self.hbbft_pk_set.public_key_share(identity.to_usize())
        {
            bail!("HBBFT private key doesn't match pubkey share");
        }
        if self.peers.keys().max().copied().map(|id| id.to_usize()) != Some(self.peers.len() - 1) {
            bail!("Peer ids are not indexed from 0");
        }
        if self.peers.keys().min().copied() != Some(PeerId::from(0)) {
            bail!("Peer ids are not indexed from 0");
        }

        let res: anyhow::Result<Vec<()>> = self
            .modules
            .iter()
            .map(|(name, cfg)| match name.as_str() {
                "wallet" => cfg
                    .to_typed::<WalletConfig>()
                    .expect("valid config should not fail")
                    .validate_config(identity),
                "mint" => cfg
                    .to_typed::<MintConfig>()
                    .expect("valid config should not fail")
                    .validate_config(identity),
                "ln" => cfg
                    .to_typed::<LightningModuleConfig>()
                    .expect("valid config should not fail")
                    .validate_config(identity),
                other => panic!("module name that was not expected: {other}"),
            })
            .collect();

        res?;

        Ok(())
    }

    pub fn trusted_dealer_gen(
        peers: &[PeerId],
        params: &HashMap<PeerId, ServerConfigParams>,
        mut rng: impl RngCore + CryptoRng,
    ) -> (BTreeMap<PeerId, Self>, ClientConfig) {
        let netinfo = hbbft::NetworkInfo::generate_map(peers.to_vec(), &mut rng)
            .expect("Could not generate HBBFT netinfo");
        let epochinfo = hbbft::NetworkInfo::generate_map(peers.to_vec(), &mut rng)
            .expect("Could not generate HBBFT netinfo");

        let peer0 = &params[&PeerId::from(0)];

        let module_cfg_gen_params = ModuleConfigGenParams {
            mint_amounts: peer0.amount_tiers.clone(),
            // TODO: FIXME: meh
            bitcoin_rpc: BitcoindRpcCfg {
                btc_rpc_address: peer0.bitcoind_rpc.clone(),
                btc_rpc_user: "bitcoin".into(),
                btc_rpc_pass: "bitcoin".into(),
            },
            other: peer0.other.clone(),
        };
        let module_config_gens: Vec<(&'static str, Box<dyn FederationModuleConfigGen>)> = vec![
            (
                "wallet",
                Box::new(WalletConfigGenerator) as Box<dyn FederationModuleConfigGen>,
            ),
            ("mint", Box::new(MintConfigGenerator)),
            ("ln", Box::new(LightningModuleConfigGen)),
        ];

        let module_configs: Vec<_> = module_config_gens
            .iter()
            .map(|(name, gen)| (name, gen.trusted_dealer_gen(peers, &module_cfg_gen_params)))
            .collect();

        let server_config = netinfo
            .iter()
            .map(|(&id, netinf)| {
                let epoch_keys = epochinfo.get(&id).unwrap();
                let config = ServerConfig {
                    federation_name: params[&id].federation_name.clone(),
                    identity: id,
                    hbbft_bind_addr: params[&id].hbbft.bind_addr.clone(),
                    api_bind_addr: params[&id].api.bind_addr.clone(),
                    tls_cert: params[&id].tls.our_certificate.clone(),
                    tls_key: params[&id].tls.our_private_key.clone(),
                    peers: params[&id].peers(),
                    hbbft_sks: SerdeSecret(netinf.secret_key_share().unwrap().clone()),
                    hbbft_pk_set: netinf.public_key_set().clone(),
                    epoch_sks: SerdeSecret(epoch_keys.secret_key_share().unwrap().clone()),
                    epoch_pk_set: epoch_keys.public_key_set().clone(),
                    modules: module_configs
                        .iter()
                        .map(|(name, cfgs)| (name.to_string(), cfgs.0[&id].clone()))
                        .collect(),
                };
                (id, config)
            })
            .collect();

        let names: HashMap<PeerId, String> = peers
            .iter()
            .map(|peer| (*peer, format!("peer-{}", peer.to_usize())))
            .collect();

        let client_config = ClientConfig {
            federation_name: peer0.federation_name.clone(),
            nodes: peer0.api.nodes("ws://", names),
            modules: module_configs
                .iter()
                .map(|(name, cfgs)| (name.to_string(), cfgs.1.clone()))
                .collect(),
        };

        (server_config, client_config)
    }

    pub async fn distributed_gen(
        connections: &ModuleMultiplexer<DkgPeerMsg>,
        our_id: &PeerId,
        peers: &[PeerId],
        params: &ServerConfigParams,
        mut rng: impl RngCore + CryptoRng,
        task_group: &mut TaskGroup,
    ) -> anyhow::Result<Cancellable<(Self, ClientConfig)>> {
        // in case we are running by ourselves, avoid DKG
        if peers.len() == 1 {
            let (server, client) =
                Self::trusted_dealer_gen(peers, &HashMap::from([(*our_id, params.clone())]), rng);
            return Ok(Ok((server[our_id].clone(), client)));
        }
        info!("Peer {} running distributed key generation...", our_id);

        // hbbft uses a lower threshold of signing keys (f+1)
        let mut dkg = DkgRunner::new(KeyType::Hbbft, peers.one_honest(), our_id, peers);
        dkg.add(KeyType::Epoch, peers.threshold());

        // run DKG for epoch and hbbft keys
        let keys = if let Ok(v) = dkg.run_g1("global", connections, &mut rng).await {
            v
        } else {
            return Ok(Err(Cancelled));
        };
        let (hbbft_pks, hbbft_sks) = keys[&KeyType::Hbbft].threshold_crypto();
        let (epoch_pks, epoch_sks) = keys[&KeyType::Epoch].threshold_crypto();

        let module_cfg_gen_params = ModuleConfigGenParams {
            mint_amounts: params.amount_tiers.clone(),
            bitcoin_rpc: BitcoindRpcCfg {
                btc_rpc_address: params.bitcoind_rpc.clone(),
                btc_rpc_user: "bitcoin".into(),
                btc_rpc_pass: "bitcoin".into(),
            },
            other: params.other.clone(),
        };
        let module_config_gens: Vec<(&'static str, Box<dyn FederationModuleConfigGen>)> = vec![
            (
                "wallet",
                Box::new(WalletConfigGenerator) as Box<dyn FederationModuleConfigGen>,
            ),
            ("mint", Box::new(MintConfigGenerator)),
            ("ln", Box::new(LightningModuleConfigGen)),
        ];

        let mut module_cfgs: Vec<(&'static str, (_, _))> = vec![];

        for (name, gen) in module_config_gens {
            module_cfgs.push((
                name,
                if let Ok(cfgs) = gen
                    .distributed_gen(
                        connections,
                        our_id,
                        peers,
                        &module_cfg_gen_params,
                        task_group,
                    )
                    .await?
                {
                    cfgs
                } else {
                    return Ok(Err(Cancelled));
                },
            ));
        }

        let server = ServerConfig {
            federation_name: params.federation_name.clone(),
            identity: *our_id,
            hbbft_bind_addr: params.hbbft.bind_addr.clone(),
            api_bind_addr: params.api.bind_addr.clone(),
            tls_cert: params.tls.our_certificate.clone(),
            tls_key: params.tls.our_private_key.clone(),
            peers: params.peers(),
            hbbft_sks: SerdeSecret(hbbft_sks),
            hbbft_pk_set: hbbft_pks,
            epoch_sks: SerdeSecret(epoch_sks),
            epoch_pk_set: epoch_pks,
            modules: module_cfgs
                .iter()
                .map(|(name, cfgs)| (name.to_string(), cfgs.0.clone()))
                .collect(),
        };

        let client = ClientConfig {
            federation_name: params.federation_name.clone(),
            nodes: params.api.nodes("ws://", params.tls.peer_names.clone()),
            modules: module_cfgs
                .iter()
                .map(|(name, cfgs)| (name.to_string(), cfgs.1.clone()))
                .collect(),
        };

        Ok(Ok((server, client)))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum KeyType {
    Hbbft,
    Epoch,
}

impl ServerConfig {
    pub fn network_config(&self) -> NetworkConfig {
        NetworkConfig {
            identity: self.identity,
            bind_addr: self.hbbft_bind_addr.clone(),
            peers: self
                .peers
                .iter()
                .map(|(&id, peer)| (id, peer.hbbft.clone()))
                .collect(),
        }
    }

    pub fn tls_config(&self) -> TlsConfig {
        TlsConfig {
            our_certificate: self.tls_cert.clone(),
            our_private_key: self.tls_key.clone(),
            peer_certs: self
                .peers
                .iter()
                .map(|(peer, cfg)| (*peer, cfg.tls_cert.clone()))
                .collect(),
            peer_names: self
                .peers
                .iter()
                .map(|(peer, cfg)| (*peer, cfg.name.to_string()))
                .collect(),
        }
    }

    pub fn get_incoming_count(&self) -> u16 {
        self.identity.into()
    }
}

pub struct PeerServerParams {
    pub cert: rustls::Certificate,
    pub address: String,
    pub base_port: u16,
    pub name: String,
}

impl ServerConfigParams {
    pub fn peers(&self) -> BTreeMap<PeerId, Peer> {
        self.hbbft
            .peers
            .iter()
            .map(|(peer, hbbft)| {
                (
                    *peer,
                    Peer {
                        name: self.tls.peer_names[peer].clone(),
                        hbbft: hbbft.clone(),
                        tls_cert: self.tls.peer_certs[peer].clone(),
                        api_addr: Url::parse(&format!("ws://{}", self.api.peers[peer].address))
                            .expect("Could not parse URL"),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>()
    }

    pub fn gen_params(
        key: rustls::PrivateKey,
        our_id: PeerId,
        amount_tiers: Vec<Amount>,
        peers: &BTreeMap<PeerId, PeerServerParams>,
        federation_name: String,
        bitcoind_rpc: String,
    ) -> ServerConfigParams {
        let peer_certs: HashMap<PeerId, rustls::Certificate> = peers
            .iter()
            .map(|(peer, params)| (*peer, params.cert.clone()))
            .collect::<HashMap<_, _>>();

        let peer_names: HashMap<PeerId, String> = peers
            .iter()
            .map(|(peer, params)| (*peer, params.name.to_string()))
            .collect::<HashMap<_, _>>();

        let tls = TlsConfig {
            our_certificate: peers[&our_id].cert.clone(),
            our_private_key: key,
            peer_certs,
            peer_names,
        };

        ServerConfigParams {
            tls,
            hbbft: Self::gen_network(&our_id, 0, peers),
            api: Self::gen_network(&our_id, 1, peers),
            server_dkg: Self::gen_network(&our_id, 2, peers),
            amount_tiers,
            federation_name,
            bitcoind_rpc,
            other: BTreeMap::new(),
        }
    }

    fn gen_network(
        our_id: &PeerId,
        offset: u16,
        peers: &BTreeMap<PeerId, PeerServerParams>,
    ) -> NetworkConfig {
        NetworkConfig {
            identity: *our_id,
            bind_addr: format!(
                "{}:{}",
                peers[our_id].address,
                peers[our_id].base_port + offset
            ),
            peers: peers
                .iter()
                .map(|(peer, params)| {
                    let connection = ConnectionConfig {
                        address: format!("{}:{}", params.address, params.base_port + offset),
                    };
                    (*peer, connection)
                })
                .collect(),
        }
    }

    /// config for servers running on different ports on a local network
    pub fn gen_local(
        peers: &[PeerId],
        amount_tiers: Vec<Amount>,
        base_port: u16,
        federation_name: &str,
        bitcoind_rpc: &str,
    ) -> HashMap<PeerId, ServerConfigParams> {
        let keys: HashMap<PeerId, (rustls::Certificate, rustls::PrivateKey)> = peers
            .iter()
            .map(|peer| {
                let (cert, key) = gen_cert_and_key(&format!("peer-{}", peer.to_usize())).unwrap();
                (*peer, (cert, key))
            })
            .collect::<HashMap<_, _>>();

        let peer_params: BTreeMap<PeerId, PeerServerParams> = peers
            .iter()
            .map(|peer| {
                let params: PeerServerParams = PeerServerParams {
                    cert: keys[peer].0.clone(),
                    address: "127.0.0.1".to_string(),
                    base_port: base_port + (u16::from(*peer) * 6),
                    name: format!("peer-{}", peer.to_usize()),
                };
                (*peer, params)
            })
            .collect();

        peers
            .iter()
            .map(|peer| {
                let params: ServerConfigParams = Self::gen_params(
                    keys[peer].1.clone(),
                    *peer,
                    amount_tiers.clone(),
                    &peer_params,
                    federation_name.to_string(),
                    bitcoind_rpc.to_string(),
                );
                (*peer, params)
            })
            .collect()
    }
}

pub async fn connect<T>(
    network: NetworkConfig,
    certs: TlsConfig,
    task_group: &mut TaskGroup,
) -> AnyPeerConnections<T>
where
    T: std::fmt::Debug + Clone + Serialize + DeserializeOwned + Unpin + Send + Sync + 'static,
{
    let connector = TlsTcpConnector::new(certs).into_dyn();
    ReconnectPeerConnections::new(network, connector, task_group)
        .await
        .into_dyn()
}

pub fn gen_cert_and_key(
    name: &str,
) -> Result<(rustls::Certificate, rustls::PrivateKey), anyhow::Error> {
    let keypair = rcgen::KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let keypair_ser = keypair.serialize_der();
    let mut params = rcgen::CertificateParams::new(vec![name.to_owned()]);

    params.key_pair = Some(keypair);
    params.alg = &rcgen::PKCS_ECDSA_P256_SHA256;
    params.is_ca = rcgen::IsCa::NoCa;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, name);

    let cert = rcgen::Certificate::from_params(params)?;

    Ok((
        rustls::Certificate(cert.serialize_der()?),
        rustls::PrivateKey(keypair_ser),
    ))
}

mod serde_tls_cert {
    use std::borrow::Cow;

    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tokio_rustls::rustls;

    pub fn serialize<S>(cert: &rustls::Certificate, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex_str = hex::encode(&cert.0);
        Serialize::serialize(&hex_str, serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<rustls::Certificate, D::Error>
    where
        D: Deserializer<'de>,
    {
        let hex_str: Cow<str> = Deserialize::deserialize(deserializer)?;
        let bytes = hex::decode(hex_str.as_ref()).map_err(|_e| D::Error::custom("Invalid hex"))?;
        Ok(rustls::Certificate(bytes))
    }
}

mod serde_tls_key {
    use std::borrow::Cow;

    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tokio_rustls::rustls;

    pub fn serialize<S>(key: &rustls::PrivateKey, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex_str = hex::encode(&key.0);
        Serialize::serialize(&hex_str, serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<rustls::PrivateKey, D::Error>
    where
        D: Deserializer<'de>,
    {
        let hex_str: Cow<str> = Deserialize::deserialize(deserializer)?;
        let bytes = hex::decode(hex_str.as_ref()).map_err(|_e| D::Error::custom("Invalid hex"))?;
        Ok(rustls::PrivateKey(bytes))
    }
}
