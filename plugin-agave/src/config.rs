use {
    crate::protobuf::ProtobufEncoder,
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPluginError, Result as PluginResult,
    },
    richat_metrics::ConfigMetrics,
    richat_shared::{
        config::{ConfigTokio, deserialize_humansize_usize, deserialize_num_str},
        transports::{grpc::ConfigGrpcServer, quic::ConfigQuicServer, shm::ConfigShmServer},
    },
    serde::{
        Deserialize,
        de::{self, Deserializer},
    },
    std::{fs, path::Path},
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub libpath: String,
    pub logs: ConfigLogs,
    pub metrics: Option<ConfigMetrics>,
    pub tokio: ConfigTokio,
    pub channel: ConfigChannel,
    pub quic: Option<ConfigQuicServer>,
    pub grpc: Option<ConfigGrpcServer>,
    pub shm: Option<ConfigShmServer>,
    /// Tokio runtime shutdown timeout in seconds (default: 10)
    #[serde(default = "Config::default_shutdown_timeout_secs")]
    pub shutdown_timeout_secs: u64,
}

impl Config {
    const fn default_shutdown_timeout_secs() -> u64 {
        10
    }

    fn load_from_str(config: &str) -> PluginResult<Self> {
        serde_json::from_str(config).map_err(|error| GeyserPluginError::ConfigFileReadError {
            msg: error.to_string(),
        })
    }

    pub fn load_from_file<P: AsRef<Path>>(file: P) -> PluginResult<Self> {
        let config = fs::read_to_string(file).map_err(GeyserPluginError::ConfigFileOpenError)?;
        Self::load_from_str(&config)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConfigLogs {
    /// Log level
    pub level: String,
}

impl Default for ConfigLogs {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConfigChannel {
    #[serde(deserialize_with = "ConfigChannel::deserialize_encoder")]
    pub encoder: ProtobufEncoder,
    #[serde(deserialize_with = "deserialize_num_str")]
    pub max_messages: usize,
    #[serde(deserialize_with = "deserialize_humansize_usize")]
    pub max_bytes: usize,
}

impl Default for ConfigChannel {
    fn default() -> Self {
        Self {
            encoder: ProtobufEncoder::Raw,
            // 2M messages (~20k/slot → ~100 slots in buffer). Must be power of 2.
            max_messages: 2_097_152,
            // 15 GiB (~150 MiB/slot → ~100 slots in buffer).
            max_bytes: 15 * 1024 * 1024 * 1024,
        }
    }
}

impl ConfigChannel {
    pub fn deserialize_encoder<'de, D>(deserializer: D) -> Result<ProtobufEncoder, D::Error>
    where
        D: Deserializer<'de>,
    {
        match Deserialize::deserialize(deserializer)? {
            "prost" => Ok(ProtobufEncoder::Prost),
            "raw" => Ok(ProtobufEncoder::Raw),
            value => Err(de::Error::custom(format!(
                "failed to decode encoder: {value}"
            ))),
        }
    }
}
