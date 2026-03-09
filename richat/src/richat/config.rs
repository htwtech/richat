use {
    richat_shared::transports::shm::ConfigShmServer,
    serde::Deserialize,
};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ConfigAppsRichat {
    pub shm: Option<ConfigShmServer>,
}
