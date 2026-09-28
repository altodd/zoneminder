//! ONVIF PTZ (phase 3): a small SOAP client for the Media and PTZ services
//! with WS-Security UsernameToken (PasswordDigest) authentication, enough
//! for pan/tilt/zoom, stop, presets and home. No WSDL code generation: the
//! requests are literal envelopes and the responses are read with a tiny
//! tag scanner (`xml_first`, `xml_all`) that ignores namespace prefixes.
//!
//! Discovery (`probe`): GetCapabilities on the device service gives the
//! Media and PTZ XAddrs, GetProfiles the first profile that carries a PTZ
//! configuration. The result is stored on the camera row so later commands
//! are one request each.

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha1::Digest;
use tracing::debug;

const WSSE: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd";
const WSU: &str = "http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd";
const DEVICE_NS: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA_NS: &str = "http://www.onvif.org/ver10/media/wsdl";
const PTZ_NS: &str = "http://www.onvif.org/ver20/ptz/wsdl";
const SCHEMA_NS: &str = "http://www.onvif.org/ver10/schema";

/// WS-Security UsernameToken header. `nonce` and `created` are parameters
/// so the digest is testable; callers use [`security_header`].
pub fn security_header_with(user: &str, pass: &str, nonce: &[u8], created: &str) -> String {
    let mut h = sha1::Sha1::new();
    h.update(nonce);
    h.update(created.as_bytes());
    h.update(pass.as_bytes());
    let digest = base64::engine::general_purpose::STANDARD.encode(h.finalize());
    let nonce_b64 = base64::engine::general_purpose::STANDARD.encode(nonce);
    format!(
        "<wsse:Security xmlns:wsse=\"{WSSE}\" xmlns:wsu=\"{WSU}\" s:mustUnderstand=\"1\"><wsse:UsernameToken>\
<wsse:Username>{}</wsse:Username>\
<wsse:Password Type=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest\">{digest}</wsse:Password>\
<wsse:Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">{nonce_b64}</wsse:Nonce>\
<wsu:Created>{created}</wsu:Created></wsse:UsernameToken></wsse:Security>",
        xml_escape(user)
    )
}

pub fn security_header(user: &str, pass: &str) -> String {
    let nonce: [u8; 16] = rand::random();
    let created = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    security_header_with(user, pass, &nonce, &created)
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn envelope(user: &str, pass: &str, body: &str) -> String {
    let header = if user.is_empty() { String::new() } else { format!("<s:Header>{}</s:Header>", security_header(user, pass)) };
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\">{header}<s:Body>{body}</s:Body></s:Envelope>")
}

/// Text of the first `<prefix:Name ...>text</prefix:Name>` element.
pub fn xml_first<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    xml_all(xml, name).into_iter().next()
}

/// Text content of every element called `name` (any namespace prefix).
pub fn xml_all<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = xml[pos..].find('<') {
        let start = pos + i;
        let rest = &xml[start + 1..];
        // element name runs to whitespace, '>' or '/'
        let end_name = rest.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(rest.len());
        let tag = &rest[..end_name];
        let local = tag.rsplit(':').next().unwrap_or(tag);
        if local == name && !tag.starts_with('/') && !tag.starts_with('?') && !tag.starts_with('!') {
            let Some(gt) = rest.find('>') else { break };
            if rest[..gt].ends_with('/') {
                out.push("");
                pos = start + 1 + gt + 1;
                continue;
            }
            let body = &rest[gt + 1..];
            let close = format!("</{tag}>");
            let Some(c) = body.find(&close) else { break };
            out.push(body[..c].trim());
            pos = start + 1 + gt + 1 + c + close.len();
        } else {
            pos = start + 1;
        }
    }
    out
}

/// The raw XML of the first element called `name` (for nested lookups).
pub fn xml_section<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let mut pos = 0;
    while let Some(i) = xml[pos..].find('<') {
        let start = pos + i;
        let rest = &xml[start + 1..];
        let end_name = rest.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(rest.len());
        let tag = &rest[..end_name];
        let local = tag.rsplit(':').next().unwrap_or(tag);
        if local == name && !tag.starts_with('/') {
            let gt = rest.find('>')?;
            if rest[..gt].ends_with('/') {
                return Some(&xml[start..start + 1 + gt + 1]); // self-closing
            }
            let close = format!("</{tag}>");
            let c = rest.find(&close)?;
            return Some(&xml[start..start + 1 + c + close.len()]);
        }
        pos = start + 1;
    }
    None
}

/// Value of `attr="..."` in the opening tag of every element called `name`.
pub fn xml_attrs<'a>(xml: &'a str, name: &str, attr: &str) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = xml[pos..].find('<') {
        let start = pos + i;
        let rest = &xml[start + 1..];
        let end_name = rest.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(rest.len());
        let tag = &rest[..end_name];
        let local = tag.rsplit(':').next().unwrap_or(tag);
        if local == name && !tag.starts_with('/') {
            let gt = rest.find('>').unwrap_or(rest.len());
            let open = &rest[..gt];
            let needle = format!("{attr}=\"");
            if let Some(a) = open.find(&needle) {
                let v = &open[a + needle.len()..];
                if let Some(q) = v.find('"') {
                    // element xml: from '<' to its closing tag; a self-closing tag ends at '>'
                    let close = format!("</{tag}>");
                    let end = if open.ends_with('/') { gt + 1 } else { rest.find(&close).map(|c| c + close.len()).unwrap_or(gt + 1) };
                    out.push((&v[..q], &xml[start..start + 1 + end]));
                }
            }
        }
        pos = start + 1;
    }
    out
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Preset {
    pub token: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Probe {
    pub media_url: String,
    pub ptz_url: String,
    pub profile: String,
}

pub struct Client {
    http: reqwest::Client,
    user: String,
    pass: String,
}

impl Client {
    pub fn new(user: &str, pass: &str) -> Result<Client> {
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(6)).redirect(reqwest::redirect::Policy::none()).build()?;
        Ok(Client { http, user: user.to_string(), pass: pass.to_string() })
    }

    async fn call(&self, url: &str, body: &str) -> Result<String> {
        let env = envelope(&self.user, &self.pass, body);
        let res = self.http.post(url).header("Content-Type", "application/soap+xml; charset=utf-8").body(env).send().await.with_context(|| format!("POST {url}"))?;
        let status = res.status();
        let text = String::from_utf8_lossy(&read_capped(res, 1 << 20).await?).to_string();
        if let Some(fault) = xml_section(&text, "Fault") {
            let reason = xml_first(fault, "Text").or_else(|| xml_first(fault, "faultstring")).unwrap_or("SOAP fault");
            anyhow::bail!("{reason} (HTTP {status})");
        }
        if !status.is_success() {
            anyhow::bail!("HTTP {status}");
        }
        debug!(%url, bytes = text.len(), "onvif reply");
        Ok(text)
    }

    /// Find the media/PTZ service addresses and a PTZ-capable profile.
    pub async fn probe(&self, device_url: &str) -> Result<Probe> {
        let caps = self.call(device_url, &format!("<tds:GetCapabilities xmlns:tds=\"{DEVICE_NS}\"><tds:Category>All</tds:Category></tds:GetCapabilities>")).await?;
        let media_url = xml_section(&caps, "Media").and_then(|s| xml_first(s, "XAddr")).ok_or_else(|| anyhow::anyhow!("camera reports no Media service"))?.to_string();
        let ptz_url = xml_section(&caps, "PTZ").and_then(|s| xml_first(s, "XAddr")).ok_or_else(|| anyhow::anyhow!("camera reports no PTZ service"))?.to_string();
        let profiles = self.call(&media_url, &format!("<trt:GetProfiles xmlns:trt=\"{MEDIA_NS}\"/>")).await?;
        let with_ptz = xml_attrs(&profiles, "Profiles", "token");
        let profile = with_ptz
            .iter()
            .find(|(_, body)| body.contains("PTZConfiguration"))
            .or_else(|| with_ptz.first())
            .map(|(t, _)| t.to_string())
            .ok_or_else(|| anyhow::anyhow!("camera reports no media profiles"))?;
        Ok(Probe { media_url, ptz_url, profile })
    }

    /// Start moving; velocities are -1..1 (0 = no movement on that axis).
    pub async fn continuous_move(&self, ptz_url: &str, profile: &str, pan: f64, tilt: f64, zoom: f64) -> Result<()> {
        let c = |v: f64| v.clamp(-1.0, 1.0);
        let body = format!(
            "<tptz:ContinuousMove xmlns:tptz=\"{PTZ_NS}\" xmlns:tt=\"{SCHEMA_NS}\"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x=\"{:.3}\" y=\"{:.3}\"/><tt:Zoom x=\"{:.3}\"/></tptz:Velocity></tptz:ContinuousMove>",
            xml_escape(profile), c(pan), c(tilt), c(zoom)
        );
        self.call(ptz_url, &body).await.map(|_| ())
    }

    pub async fn stop(&self, ptz_url: &str, profile: &str) -> Result<()> {
        let body = format!("<tptz:Stop xmlns:tptz=\"{PTZ_NS}\"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>true</tptz:Zoom></tptz:Stop>", xml_escape(profile));
        self.call(ptz_url, &body).await.map(|_| ())
    }

    pub async fn presets(&self, ptz_url: &str, profile: &str) -> Result<Vec<Preset>> {
        let body = format!("<tptz:GetPresets xmlns:tptz=\"{PTZ_NS}\"><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetPresets>", xml_escape(profile));
        let xml = self.call(ptz_url, &body).await?;
        Ok(xml_attrs(&xml, "Preset", "token")
            .into_iter()
            .map(|(t, el)| Preset { token: t.to_string(), name: xml_first(el, "Name").unwrap_or(t).to_string() })
            .collect())
    }

    pub async fn goto_preset(&self, ptz_url: &str, profile: &str, token: &str) -> Result<()> {
        let body = format!("<tptz:GotoPreset xmlns:tptz=\"{PTZ_NS}\"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken></tptz:GotoPreset>", xml_escape(profile), xml_escape(token));
        self.call(ptz_url, &body).await.map(|_| ())
    }

    pub async fn set_preset(&self, ptz_url: &str, profile: &str, name: &str) -> Result<String> {
        let body = format!("<tptz:SetPreset xmlns:tptz=\"{PTZ_NS}\"><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetName>{}</tptz:PresetName></tptz:SetPreset>", xml_escape(profile), xml_escape(name));
        let xml = self.call(ptz_url, &body).await?;
        Ok(xml_first(&xml, "PresetToken").unwrap_or("").to_string())
    }

    pub async fn goto_home(&self, ptz_url: &str, profile: &str) -> Result<()> {
        let body = format!("<tptz:GotoHomePosition xmlns:tptz=\"{PTZ_NS}\"><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GotoHomePosition>", xml_escape(profile));
        self.call(ptz_url, &body).await.map(|_| ())
    }
}

/// Read a response body with a hard size cap (cameras and detectors must
/// never make the server buffer an arbitrary amount).
pub async fn read_capped(res: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    use futures::StreamExt;
    if res.content_length().is_some_and(|n| n as usize > max) {
        anyhow::bail!("response too large");
    }
    let mut out = Vec::new();
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() + chunk.len() > max {
            anyhow::bail!("response too large");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// ONVIF credentials for a camera: its own, else the RTSP URL's (percent
/// decoded, as the recorder does for RTSP).
pub fn credentials(cam: &crate::db::Camera) -> (String, String) {
    if !cam.onvif_user.is_empty() {
        return (cam.onvif_user.clone(), cam.onvif_pass.clone());
    }
    let dec = |s: &str| percent_encoding::percent_decode_str(s).decode_utf8_lossy().to_string();
    match url::Url::parse(&cam.main_url) {
        Ok(u) if !u.username().is_empty() => (dec(u.username()), dec(u.password().unwrap_or(""))),
        _ => (String::new(), String::new()),
    }
}

/// Default device-service URL from the RTSP host when none is configured.
pub fn default_device_url(cam: &crate::db::Camera) -> Option<String> {
    if !cam.onvif_url.is_empty() {
        return Some(cam.onvif_url.clone());
    }
    let u = url::Url::parse(&cam.main_url).ok()?;
    Some(format!("http://{}/onvif/device_service", u.host_str()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_the_ws_security_recipe() {
        // known vector: sha1(nonce + created + password), nonce bytes = "abcd"
        let h = security_header_with("admin", "secret", b"abcd", "2026-09-28T10:00:00.000Z");
        let mut sha = sha1::Sha1::new();
        sha.update(b"abcd");
        sha.update(b"2026-09-28T10:00:00.000Z");
        sha.update(b"secret");
        let want = base64::engine::general_purpose::STANDARD.encode(sha.finalize());
        assert!(h.contains(&format!("#PasswordDigest\">{want}</wsse:Password>")), "{h}");
        assert!(h.contains("<wsse:Nonce EncodingType=\"http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary\">YWJjZA==</wsse:Nonce>"));
        assert!(h.contains("<wsu:Created>2026-09-28T10:00:00.000Z</wsu:Created>"));
        assert!(security_header_with("a<b", "x", b"n", "t").contains("<wsse:Username>a&lt;b</wsse:Username>"));
    }

    #[test]
    fn xml_scanner_ignores_prefixes_and_finds_attributes() {
        let xml = r#"<?xml version="1.0"?><s:Envelope><s:Body><trt:GetProfilesResponse>
            <trt:Profiles token="P1" fixed="true"><tt:Name>main</tt:Name><tt:VideoEncoderConfiguration/></trt:Profiles>
            <trt:Profiles token="P2"><tt:Name>ptz</tt:Name><tt:PTZConfiguration token="ptzc"><tt:Name>c</tt:Name></tt:PTZConfiguration></trt:Profiles>
            </trt:GetProfilesResponse></s:Body></s:Envelope>"#;
        assert_eq!(xml_all(xml, "Name"), vec!["main", "ptz", "c"]);
        assert_eq!(xml_first(xml, "Name"), Some("main"));
        let p = xml_attrs(xml, "Profiles", "token");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].0, "P1");
        assert!(!p[0].1.contains("PTZConfiguration"));
        assert!(p[1].1.contains("PTZConfiguration"));
        // a self-closing profile does not swallow its PTZ sibling
        let sc = xml_attrs("<r><trt:Profiles token=\"A\"/><trt:Profiles token=\"B\"><tt:PTZConfiguration/></trt:Profiles></r>", "Profiles", "token");
        assert!(!sc[0].1.contains("PTZConfiguration") && sc[1].1.contains("PTZConfiguration"));
        assert!(xml_section(xml, "PTZConfiguration").unwrap().starts_with("<tt:PTZConfiguration"));
        assert_eq!(xml_first(xml, "Missing"), None);
        let caps = "<tt:Media><tt:XAddr>http://c/onvif/Media</tt:XAddr></tt:Media><tt:PTZ><tt:XAddr>http://c/onvif/PTZ</tt:XAddr></tt:PTZ>";
        assert_eq!(xml_section(caps, "PTZ").and_then(|s| xml_first(s, "XAddr")), Some("http://c/onvif/PTZ"));
        assert_eq!(xml_first("<a><b/></a>", "b"), Some(""));
        assert_eq!(xml_section("<v><tt:PanTilt x=\"0.5\" y=\"-1\"/><tt:Zoom x=\"0\"/></v>", "PanTilt"), Some("<tt:PanTilt x=\"0.5\" y=\"-1\"/>"));
        assert_eq!(xml_section("<v><tt:PanTilt x=\"0.5\"/><tt:Zoom x=\"0\"/></v>", "Zoom"), Some("<tt:Zoom x=\"0\"/>"));
    }

    #[test]
    fn credentials_fall_back_to_the_rtsp_url() {
        let mut cam = crate::db::Camera::example();
        cam.main_url = "rtsp://joe:pw%40x@10.0.0.9:554/s".into();
        assert_eq!(credentials(&cam), ("joe".into(), "pw@x".into()));
        assert_eq!(default_device_url(&cam).as_deref(), Some("http://10.0.0.9/onvif/device_service"));
        cam.onvif_user = "onvif".into();
        cam.onvif_pass = "p".into();
        cam.onvif_url = "http://10.0.0.9:8080/onvif/device_service".into();
        assert_eq!(credentials(&cam), ("onvif".into(), "p".into()));
        assert_eq!(default_device_url(&cam).as_deref(), Some("http://10.0.0.9:8080/onvif/device_service"));
    }
}
