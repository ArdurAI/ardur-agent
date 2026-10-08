//! Paired-home client only. No bot execution, database, or provider dependencies.
mod commands;
pub use commands::{CommandResult, DeviceCommand, execute_device, safe_output};
mod storage;
mod transport;
use home_protocol::{
    DeviceKeys, HomePins, Identity, Proof, canonical_json, decode_pairing_code, device_signed_text,
    https_url, nonce, pairing_signed_text, sign_text, string_len, valid_nonce, verify_home,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub use storage::{FileStore, SecretStore, default_config_dir};
pub use transport::PinnedTransport;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Input,
    InvalidUnicode,
    Storage,
    Identity,
    Unreachable,
    Protocol,
    Access,
    Expired,
    RequestChanged,
}
impl Error {
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Input | Self::InvalidUnicode | Self::RequestChanged => 3,
            Self::Storage | Self::Identity | Self::Access => 2,
            _ => 1,
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Self::Input => "invalid_input",
            Self::InvalidUnicode => "invalid_unicode",
            Self::Storage => "unsafe_storage",
            Self::Identity => "home_changed",
            Self::Unreachable => "home_unreachable",
            Self::Protocol => "protocol_failure",
            Self::Access => "access_refused",
            Self::Expired => "expired_nonce",
            Self::RequestChanged => "request_changed",
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidUnicode => "JSON strings must contain well-formed Unicode; unpaired surrogates are not allowed.",
            Self::RequestChanged => "This request changed; send it as a new task.",
            Self::Input => "Check the arguments or copy a new pairing code from Settings, Devices.",
            Self::Storage => {
                "Private pairing storage is unavailable or unsafe. Check its owner and permissions."
            }
            Self::Identity => "This home's identity changed; pair this device again.",
            Self::Unreachable => "Home unreachable. Check that Ardur is running.",
            Self::Protocol => "Home could not finish this request; try again.",
            Self::Access => {
                "This action is unavailable from this device. Check permissions at home."
            }
            Self::Expired => "Home returned an expired request; try again.",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Profile {
    pub schema_version: u8,
    pub url: String,
    pub home_name: String,
    pub pins: HomePins,
    pub grant_id: String,
    pub space_id: String,
}
pub struct StoredHome {
    pub profile: Profile,
    pub private_key: Zeroizing<String>,
}
impl StoredHome {
    pub fn validate(&self) -> Result<(), Error> {
        let p = &self.profile;
        if p.schema_version != 1
            || https_url(&p.url)
                .map_err(|_| Error::Storage)?
                .origin()
                .ascii_serialization()
                != p.url
            || !(1..=80).contains(&string_len(&p.home_name))
            || !(1..=128).contains(&string_len(&p.pins.instance_id))
            || !(1..=128).contains(&string_len(&p.grant_id))
            || !(1..=128).contains(&string_len(&p.space_id))
            || !home_protocol::fingerprint_valid(&p.pins.fingerprint)
            || !home_protocol::fingerprint_valid(&p.pins.certificate_fingerprint)
        {
            return Err(Error::Storage);
        }
        sign_text(&self.private_key, "key validation").map_err(|_| Error::Storage)?;
        Ok(())
    }
}
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
async fn hello(url: &str, pins: &HomePins, grant: Option<&str>) -> Result<Identity, Error> {
    let challenge = nonce();
    let mut body = json!({"clientChallenge":challenge,"purpose":"request"});
    if let Some(g) = grant {
        body["grantId"] = json!(g)
    }
    let result = PinnedTransport::new(&pins.certificate_fingerprint)?
        .post(&format!("{url}/device/nonce"), &body)
        .await?;
    let identity: Identity = serde_json::from_value(result).map_err(|_| Error::Identity)?;
    if !verify_home(pins, &challenge, &identity, now_ms()) {
        return Err(Error::Identity);
    }
    Ok(identity)
}
pub async fn pair_device(code: &str, name: &str) -> Result<StoredHome, Error> {
    let p = decode_pairing_code(code).map_err(|error| match error {
        home_protocol::InvalidProtocol::InvalidUnicode => Error::InvalidUnicode,
        home_protocol::InvalidProtocol::InvalidData => Error::Input,
    })?;
    if !(1..=80).contains(&string_len(name.trim())) {
        return Err(Error::Input);
    }
    let hint = p.hints.first().ok_or(Error::Input)?;
    if https_url(hint)
        .map_err(|_| Error::Input)?
        .origin()
        .ascii_serialization()
        != *hint
    {
        return Err(Error::Input);
    }
    hello(hint, &p.pins(), None).await?;
    let keys = DeviceKeys::generate().map_err(|_| Error::Protocol)?;
    let signature = keys
        .sign(&pairing_signed_text(
            &p,
            &keys.public_key,
            &keys.presence_public_key,
        ))
        .map_err(|_| Error::Protocol)?;
    let result=PinnedTransport::new(&p.certificate_fingerprint)?.post(&format!("{hint}/device/pair"),&json!({
        "challenge":p.challenge,"instanceId":p.instance_id,"deviceName":name.trim(),"platform":"cli",
        "devicePublicKey":keys.public_key,"presencePublicKey":keys.presence_public_key,"signature":signature
    })).await?;
    let string = |field: &str| {
        result
            .get(field)
            .and_then(Value::as_str)
            .filter(|v| (1..=128).contains(&string_len(v)))
            .map(str::to_owned)
            .ok_or(Error::Protocol)
    };
    let grant_id = string("grantId")?;
    let space_id = string("spaceId")?;
    if result["instanceId"] != p.instance_id {
        return Err(Error::Protocol);
    }
    Ok(StoredHome {
        profile: Profile {
            schema_version: 1,
            url: hint.clone(),
            home_name: p.home_name.clone(),
            pins: p.pins(),
            grant_id,
            space_id,
        },
        private_key: keys.private_key,
    })
}
pub struct HomeClient {
    home: StoredHome,
}
impl HomeClient {
    pub fn new(home: StoredHome) -> Result<Self, Error> {
        home.validate()?;
        Ok(Self { home })
    }
    pub async fn request(&self, operation: &str, body: &Value) -> Result<Value, Error> {
        let p = &self.home.profile;
        let i = hello(&p.url, &p.pins, Some(&p.grant_id)).await?;
        if !valid_nonce(&i, now_ms()) {
            return Err(Error::Expired);
        }
        let mut proof = Proof {
            grant_id: p.grant_id.clone(),
            nonce: i.nonce.ok_or(Error::Expired)?,
            timestamp: i.timestamp.ok_or(Error::Expired)?,
            signature: String::new(),
        };
        proof.signature = sign_text(
            &self.home.private_key,
            &device_signed_text(&p.pins.instance_id, &proof, operation, body),
        )
        .map_err(|_| Error::Storage)?;
        PinnedTransport::new(&p.pins.certificate_fingerprint)?
            .post(
                &format!("{}/device/request", p.url),
                &json!({"operation":operation,"body":body,"proof":proof}),
            )
            .await
    }
    pub async fn status(&self) -> Result<Value, Error> {
        let tasks = self.request("tasks", &json!({})).await?;
        if !tasks.is_array() {
            return Err(Error::Protocol);
        }
        Ok(
            json!({"homeName":self.home.profile.home_name,"instanceId":self.home.profile.pins.instance_id,"valid":true}),
        )
    }
    pub async fn bots(&self) -> Result<Value, Error> {
        let value = self
            .request("rpc", &json!({"procedure":"bots/list","input":{}}))
            .await?;
        let bots = value
            .as_array()
            .ok_or(Error::Protocol)?
            .iter()
            .map(|v| -> Result<Value, Error> {
                let mut b = serde_json::Map::new();
                for f in ["id", "name", "threadId", "status"] {
                    let s = v.get(f).and_then(Value::as_str).ok_or(Error::Protocol)?;
                    b.insert(f.into(), json!(s));
                }
                for f in ["modelProvider", "modelId", "thinkingLevel"] {
                    let s = v.get(f).ok_or(Error::Protocol)?;
                    if !s.is_null() && !s.is_string() {
                        return Err(Error::Protocol);
                    }
                    b.insert(f.into(), s.clone());
                }
                let runtime = v.get("runtimeKind").cloned().unwrap_or(json!("pi"));
                if !runtime.is_string() {
                    return Err(Error::Protocol);
                }
                b.insert("runtimeKind".into(), runtime);
                Ok(Value::Object(b))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({"bots":bots}))
    }
}
/// Escape controls (including C1 and bidi format controls) instead of emitting terminal commands.
pub fn human_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}') {
                format!("\\u{{{:x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}
pub fn encode_body(body: &Value) -> String {
    canonical_json(body)
}
