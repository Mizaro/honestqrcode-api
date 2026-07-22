//! Deterministic QR payload construction and rendering.
//!
//! The public interface is deliberately small: callers provide a [`QrSpec`] to
//! [`render`] and receive a [`QrArtifact`]. Validation, payload encoding,
//! symbol sizing, rasterization, vector rendering, and hashing stay inside this
//! module so every transport adapter behaves identically.

use std::fmt::Write as FmtWrite;
use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use qrcode::{EcLevel, QrCode, types::Color as ModuleColor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::{Url, form_urlencoded};
use utoipa::ToSchema;

pub const MAX_PAYLOAD_BYTES: usize = 2_953;
pub const MIN_WIDTH: u32 = 64;
pub const MAX_WIDTH: u32 = 4_096;
pub const MAX_MARGIN: u8 = 32;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QrSpec {
    pub data: QrData,
    #[serde(default)]
    pub render: RenderOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QrData {
    Text {
        value: String,
    },
    Url {
        url: String,
    },
    Bytes {
        base64: String,
    },
    Wifi {
        ssid: String,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        security: WifiSecurity,
        #[serde(default)]
        hidden: bool,
    },
    Email {
        to: String,
        #[serde(default)]
        subject: Option<String>,
        #[serde(default)]
        body: Option<String>,
    },
    Phone {
        number: String,
    },
    Sms {
        number: String,
        #[serde(default)]
        message: Option<String>,
    },
    Whatsapp {
        number: String,
        #[serde(default)]
        message: Option<String>,
    },
    Vcard {
        first_name: String,
        last_name: String,
        #[serde(default)]
        organization: Option<String>,
        #[serde(default)]
        email: Option<String>,
        #[serde(default)]
        phone: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
    Location {
        latitude: f64,
        longitude: f64,
        #[serde(default)]
        label: Option<String>,
    },
    Event {
        title: String,
        start: String,
        #[serde(default)]
        end: Option<String>,
        #[serde(default)]
        location: Option<String>,
        #[serde(default)]
        description: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum WifiSecurity {
    #[default]
    Wpa,
    Wep,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RenderOptions {
    #[serde(default)]
    pub format: QrFormat,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_margin")]
    pub margin: u8,
    #[serde(default)]
    pub error_correction: ErrorCorrection,
    #[serde(default = "default_foreground")]
    pub foreground: String,
    #[serde(default = "default_background")]
    pub background: String,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            format: QrFormat::Png,
            width: default_width(),
            margin: default_margin(),
            error_correction: ErrorCorrection::Medium,
            foreground: default_foreground(),
            background: default_background(),
        }
    }
}

const fn default_width() -> u32 {
    512
}

const fn default_margin() -> u8 {
    4
}

fn default_foreground() -> String {
    "#000000".to_owned()
}

fn default_background() -> String {
    "#ffffff".to_owned()
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum QrFormat {
    #[default]
    Png,
    Svg,
    Matrix,
}

impl QrFormat {
    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Svg => "image/svg+xml; charset=utf-8",
            Self::Matrix => "application/json; charset=utf-8",
        }
    }

    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Svg => "svg",
            Self::Matrix => "json",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ErrorCorrection {
    Low,
    #[default]
    Medium,
    Quartile,
    High,
}

impl From<ErrorCorrection> for EcLevel {
    fn from(value: ErrorCorrection) -> Self {
        match value {
            ErrorCorrection::Low => Self::L,
            ErrorCorrection::Medium => Self::M,
            ErrorCorrection::Quartile => Self::Q,
            ErrorCorrection::High => Self::H,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct QrMetadata {
    pub content_type: String,
    pub extension: String,
    pub width: u32,
    pub height: u32,
    pub modules: u32,
    pub version: u8,
    pub payload_bytes: usize,
    pub sha256: String,
}

#[derive(Debug, Clone)]
pub struct QrArtifact {
    pub bytes: Vec<u8>,
    pub metadata: QrMetadata,
}

#[derive(Debug, Error)]
pub enum QrError {
    #[error("payload must not be empty")]
    EmptyPayload,
    #[error("payload is {actual} bytes; maximum is {maximum}")]
    PayloadTooLarge { actual: usize, maximum: usize },
    #[error("width must be between {MIN_WIDTH} and {MAX_WIDTH} pixels")]
    InvalidWidth,
    #[error("margin must not exceed {MAX_MARGIN} modules")]
    InvalidMargin,
    #[error("invalid {field}: {reason}")]
    InvalidField { field: &'static str, reason: String },
    #[error("the requested width is too small for this payload and margin")]
    WidthTooSmall,
    #[error("payload cannot fit in a QR Code at this error-correction level")]
    DataOverflow,
    #[error("failed to render {format} output")]
    RenderFailed { format: &'static str },
}

impl QrError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::EmptyPayload => "empty_payload",
            Self::PayloadTooLarge { .. } => "payload_too_large",
            Self::InvalidWidth => "invalid_width",
            Self::InvalidMargin => "invalid_margin",
            Self::InvalidField { .. } => "invalid_field",
            Self::WidthTooSmall => "width_too_small",
            Self::DataOverflow => "data_overflow",
            Self::RenderFailed { .. } => "render_failed",
        }
    }
}

/// Render a QR artifact from a complete, transport-independent specification.
pub fn render(spec: &QrSpec) -> Result<QrArtifact, QrError> {
    validate_render_options(&spec.render)?;
    let payload = payload_bytes(&spec.data)?;
    if payload.is_empty() {
        return Err(QrError::EmptyPayload);
    }
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(QrError::PayloadTooLarge {
            actual: payload.len(),
            maximum: MAX_PAYLOAD_BYTES,
        });
    }

    let code = QrCode::with_error_correction_level(&payload, spec.render.error_correction.into())
        .map_err(|_| QrError::DataOverflow)?;
    let module_count = u32::try_from(code.width()).map_err(|_| QrError::DataOverflow)?;
    let total_modules = module_count + (u32::from(spec.render.margin) * 2);
    let scale = spec.render.width / total_modules;
    if scale == 0 {
        return Err(QrError::WidthTooSmall);
    }
    let actual_width = total_modules * scale;
    let foreground = parse_color("foreground", &spec.render.foreground)?;
    let background = parse_color("background", &spec.render.background)?;

    if foreground == background {
        return Err(QrError::InvalidField {
            field: "colors",
            reason: "foreground and background must differ".to_owned(),
        });
    }

    let bytes = match spec.render.format {
        QrFormat::Png => render_png(
            &code,
            module_count,
            actual_width,
            scale,
            spec.render.margin,
            foreground,
            background,
        )?,
        QrFormat::Svg => render_svg(
            &code,
            module_count,
            actual_width,
            spec.render.margin,
            &spec.render.foreground,
            &spec.render.background,
        )
        .into_bytes(),
        QrFormat::Matrix => render_matrix(&code, module_count)?,
    };

    let sha256 = hex::encode(Sha256::digest(&bytes));
    let version = u8::try_from((module_count.saturating_sub(17)) / 4)
        .map_err(|_| QrError::RenderFailed { format: "metadata" })?;

    Ok(QrArtifact {
        bytes,
        metadata: QrMetadata {
            content_type: spec.render.format.content_type().to_owned(),
            extension: spec.render.format.extension().to_owned(),
            width: actual_width,
            height: actual_width,
            modules: module_count,
            version,
            payload_bytes: payload.len(),
            sha256,
        },
    })
}

fn validate_render_options(options: &RenderOptions) -> Result<(), QrError> {
    if !(MIN_WIDTH..=MAX_WIDTH).contains(&options.width) {
        return Err(QrError::InvalidWidth);
    }
    if options.margin > MAX_MARGIN {
        return Err(QrError::InvalidMargin);
    }
    Ok(())
}

fn payload_bytes(data: &QrData) -> Result<Vec<u8>, QrError> {
    let payload = match data {
        QrData::Text { value } => value.as_bytes().to_vec(),
        QrData::Url { url } => {
            let parsed = Url::parse(url).map_err(|error| invalid("url", error))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(QrError::InvalidField {
                    field: "url",
                    reason: "only http and https URLs are accepted".to_owned(),
                });
            }
            parsed.as_str().as_bytes().to_vec()
        }
        QrData::Bytes { base64 } => {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(base64)
                .map_err(|error| invalid("base64", error))?
        }
        QrData::Wifi {
            ssid,
            password,
            security,
            hidden,
        } => {
            require_nonempty("ssid", ssid)?;
            if !matches!(security, WifiSecurity::None)
                && password.as_deref().unwrap_or_default().is_empty()
            {
                return Err(QrError::InvalidField {
                    field: "password",
                    reason: "a password is required for WPA and WEP networks".to_owned(),
                });
            }
            let security = match security {
                WifiSecurity::Wpa => "WPA",
                WifiSecurity::Wep => "WEP",
                WifiSecurity::None => "nopass",
            };
            format!(
                "WIFI:T:{security};S:{};P:{};H:{};;",
                escape_wifi(ssid),
                escape_wifi(password.as_deref().unwrap_or_default()),
                hidden
            )
            .into_bytes()
        }
        QrData::Email { to, subject, body } => {
            validate_email(to)?;
            let mut serializer = form_urlencoded::Serializer::new(String::new());
            if let Some(subject) = nonempty(subject.as_deref()) {
                serializer.append_pair("subject", subject);
            }
            if let Some(body) = nonempty(body.as_deref()) {
                serializer.append_pair("body", body);
            }
            let query = serializer.finish();
            if query.is_empty() {
                format!("mailto:{to}")
            } else {
                format!("mailto:{to}?{query}")
            }
            .into_bytes()
        }
        QrData::Phone { number } => format!("tel:{}", normalize_phone(number)?).into_bytes(),
        QrData::Sms { number, message } => {
            let number = normalize_phone(number)?;
            if let Some(message) = nonempty(message.as_deref()) {
                let query = form_urlencoded::Serializer::new(String::new())
                    .append_pair("body", message)
                    .finish();
                format!("sms:{number}?{query}")
            } else {
                format!("sms:{number}")
            }
            .into_bytes()
        }
        QrData::Whatsapp { number, message } => {
            let number = normalize_phone(number)?.trim_start_matches('+').to_owned();
            if let Some(message) = nonempty(message.as_deref()) {
                let query = form_urlencoded::Serializer::new(String::new())
                    .append_pair("text", message)
                    .finish();
                format!("https://wa.me/{number}?{query}")
            } else {
                format!("https://wa.me/{number}")
            }
            .into_bytes()
        }
        QrData::Vcard {
            first_name,
            last_name,
            organization,
            email,
            phone,
            url,
        } => {
            if first_name.trim().is_empty() && last_name.trim().is_empty() {
                return Err(QrError::InvalidField {
                    field: "vcard name",
                    reason: "first_name or last_name is required".to_owned(),
                });
            }
            if let Some(email) = nonempty(email.as_deref()) {
                validate_email(email)?;
            }
            if let Some(url) = nonempty(url.as_deref()) {
                let parsed = Url::parse(url).map_err(|error| invalid("vcard url", error))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(QrError::InvalidField {
                        field: "vcard url",
                        reason: "only http and https URLs are accepted".to_owned(),
                    });
                }
            }

            let mut lines = vec![
                "BEGIN:VCARD".to_owned(),
                "VERSION:3.0".to_owned(),
                format!(
                    "N:{};{};;;",
                    escape_vcard(last_name),
                    escape_vcard(first_name)
                ),
                format!(
                    "FN:{}",
                    escape_vcard(format!("{first_name} {last_name}").trim())
                ),
            ];
            push_vcard(&mut lines, "ORG", organization.as_deref());
            push_vcard(&mut lines, "EMAIL", email.as_deref());
            push_vcard(&mut lines, "TEL", phone.as_deref());
            push_vcard(&mut lines, "URL", url.as_deref());
            lines.push("END:VCARD".to_owned());
            lines.join("\r\n").into_bytes()
        }
        QrData::Location {
            latitude,
            longitude,
            label,
        } => {
            if !(-90.0..=90.0).contains(latitude) {
                return Err(QrError::InvalidField {
                    field: "latitude",
                    reason: "must be between -90 and 90".to_owned(),
                });
            }
            if !(-180.0..=180.0).contains(longitude) {
                return Err(QrError::InvalidField {
                    field: "longitude",
                    reason: "must be between -180 and 180".to_owned(),
                });
            }
            if let Some(label) = nonempty(label.as_deref()) {
                format!(
                    "geo:{latitude},{longitude}?q={latitude},{longitude}({})",
                    percent_encode_component(label)
                )
            } else {
                format!("geo:{latitude},{longitude}")
            }
            .into_bytes()
        }
        QrData::Event {
            title,
            start,
            end,
            location,
            description,
        } => {
            require_nonempty("title", title)?;
            validate_calendar_time("start", start)?;
            if let Some(end) = nonempty(end.as_deref()) {
                validate_calendar_time("end", end)?;
                if end < start.as_str() {
                    return Err(QrError::InvalidField {
                        field: "end",
                        reason: "must not be before start".to_owned(),
                    });
                }
            }
            let mut lines = vec![
                "BEGIN:VCALENDAR".to_owned(),
                "VERSION:2.0".to_owned(),
                "PRODID:-//Honest QR Code//API//EN".to_owned(),
                "BEGIN:VEVENT".to_owned(),
                format!("SUMMARY:{}", escape_vcard(title)),
                format!("DTSTART:{}", normalize_calendar_time(start)),
            ];
            if let Some(end) = nonempty(end.as_deref()) {
                lines.push(format!("DTEND:{}", normalize_calendar_time(end)));
            }
            push_vcard(&mut lines, "LOCATION", location.as_deref());
            push_vcard(&mut lines, "DESCRIPTION", description.as_deref());
            lines.push("END:VEVENT".to_owned());
            lines.push("END:VCALENDAR".to_owned());
            lines.join("\r\n").into_bytes()
        }
    };
    Ok(payload)
}

fn render_png(
    code: &QrCode,
    module_count: u32,
    actual_width: u32,
    scale: u32,
    margin: u8,
    foreground: Rgba<u8>,
    background: Rgba<u8>,
) -> Result<Vec<u8>, QrError> {
    let mut image = RgbaImage::from_pixel(actual_width, actual_width, background);
    let margin_pixels = u32::from(margin) * scale;
    let colors = code.to_colors();

    for y in 0..module_count {
        for x in 0..module_count {
            let index = usize::try_from(y * module_count + x)
                .map_err(|_| QrError::RenderFailed { format: "PNG" })?;
            if colors[index] != ModuleColor::Dark {
                continue;
            }
            let start_x = margin_pixels + (x * scale);
            let start_y = margin_pixels + (y * scale);
            for pixel_y in start_y..start_y + scale {
                for pixel_x in start_x..start_x + scale {
                    image.put_pixel(pixel_x, pixel_y, foreground);
                }
            }
        }
    }

    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|_| QrError::RenderFailed { format: "PNG" })?;
    Ok(output.into_inner())
}

fn render_svg(
    code: &QrCode,
    module_count: u32,
    actual_width: u32,
    margin: u8,
    foreground: &str,
    background: &str,
) -> String {
    let total = module_count + (u32::from(margin) * 2);
    let colors = code.to_colors();
    let mut path = String::with_capacity(colors.len() * 4);

    for y in 0..module_count {
        let mut x = 0;
        while x < module_count {
            let index = usize::try_from(y * module_count + x).unwrap_or_default();
            if colors[index] != ModuleColor::Dark {
                x += 1;
                continue;
            }
            let start = x;
            while x < module_count {
                let run_index = usize::try_from(y * module_count + x).unwrap_or_default();
                if colors[run_index] != ModuleColor::Dark {
                    break;
                }
                x += 1;
            }
            let run = x - start;
            let _ = write!(
                path,
                "M{} {}h{}v1h-{}z",
                start + u32::from(margin),
                y + u32::from(margin),
                run,
                run
            );
        }
    }

    format!(
        concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" ",
            "width=\"{}\" height=\"{}\" ",
            "viewBox=\"0 0 {} {}\" shape-rendering=\"crispEdges\" ",
            "role=\"img\" aria-label=\"QR code\">",
            "<rect width=\"{}\" height=\"{}\" fill=\"{}\"/>",
            "<path d=\"{}\" fill=\"{}\"/>",
            "</svg>"
        ),
        actual_width, actual_width, total, total, total, total, background, path, foreground
    )
}

fn render_matrix(code: &QrCode, module_count: u32) -> Result<Vec<u8>, QrError> {
    #[derive(Serialize)]
    struct Matrix<'a> {
        width: u32,
        dark: Vec<bool>,
        order: &'a str,
    }

    let dark = code
        .to_colors()
        .into_iter()
        .map(|color| color == ModuleColor::Dark)
        .collect();
    serde_json::to_vec(&Matrix {
        width: module_count,
        dark,
        order: "row_major",
    })
    .map_err(|_| QrError::RenderFailed {
        format: "matrix JSON",
    })
}

fn parse_color(field: &'static str, value: &str) -> Result<Rgba<u8>, QrError> {
    let hex = value.strip_prefix('#').unwrap_or(value);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(QrError::InvalidField {
            field,
            reason: "expected a six-digit hexadecimal color such as #000000".to_owned(),
        });
    }
    let bytes = hex::decode(hex).map_err(|error| invalid(field, error))?;
    Ok(Rgba([bytes[0], bytes[1], bytes[2], 255]))
}

fn normalize_phone(value: &str) -> Result<String, QrError> {
    let trimmed = value.trim();
    let has_plus = trimmed.starts_with('+');
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    if !(7..=15).contains(&digits.len()) {
        return Err(QrError::InvalidField {
            field: "number",
            reason: "expected 7 to 15 digits".to_owned(),
        });
    }
    Ok(if has_plus {
        format!("+{digits}")
    } else {
        digits
    })
}

fn validate_email(value: &str) -> Result<(), QrError> {
    let trimmed = value.trim();
    let (local, domain) = trimmed.split_once('@').unwrap_or_default();
    if local.is_empty()
        || domain.is_empty()
        || domain.starts_with('.')
        || domain.ends_with('.')
        || !domain.contains('.')
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err(QrError::InvalidField {
            field: "email",
            reason: "expected a valid email address".to_owned(),
        });
    }
    Ok(())
}

fn validate_calendar_time(field: &'static str, value: &str) -> Result<(), QrError> {
    let compact = normalize_calendar_time(value);
    let valid = matches!(compact.len(), 8 | 15 | 16)
        && compact
            .chars()
            .enumerate()
            .all(|(index, character)| match (compact.len(), index) {
                (15 | 16, 8) => character == 'T',
                (16, 15) => character == 'Z',
                _ => character.is_ascii_digit(),
            });
    if !valid {
        return Err(QrError::InvalidField {
            field,
            reason: "expected YYYYMMDD, YYYYMMDDTHHMMSS, or an ISO-like equivalent".to_owned(),
        });
    }
    Ok(())
}

fn normalize_calendar_time(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|character| !matches!(character, '-' | ':' | ' '))
        .collect()
}

fn percent_encode_component(value: &str) -> String {
    form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn escape_wifi(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | ';' | ',' | ':' | '"') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn escape_vcard(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace("\r\n", "\\n")
        .replace(['\r', '\n'], "\\n")
}

fn push_vcard(lines: &mut Vec<String>, name: &str, value: Option<&str>) {
    if let Some(value) = nonempty(value) {
        lines.push(format!("{name}:{}", escape_vcard(value)));
    }
}

fn require_nonempty(field: &'static str, value: &str) -> Result<(), QrError> {
    if value.trim().is_empty() {
        return Err(QrError::InvalidField {
            field,
            reason: "must not be empty".to_owned(),
        });
    }
    Ok(())
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn invalid(field: &'static str, error: impl std::fmt::Display) -> QrError {
    QrError::InvalidField {
        field,
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_spec(format: QrFormat) -> QrSpec {
        QrSpec {
            data: QrData::Text {
                value: "https://honestqrcode.com/".to_owned(),
            },
            render: RenderOptions {
                format,
                ..RenderOptions::default()
            },
        }
    }

    #[test]
    fn png_is_valid_and_deterministic() {
        let first = render(&text_spec(QrFormat::Png)).expect("first render");
        let second = render(&text_spec(QrFormat::Png)).expect("second render");
        assert_eq!(first.bytes, second.bytes);
        assert_eq!(first.metadata.sha256, second.metadata.sha256);
        assert_eq!(&first.bytes[..8], b"\x89PNG\r\n\x1a\n");
        image::load_from_memory_with_format(&first.bytes, ImageFormat::Png).expect("PNG decodes");
    }

    #[test]
    fn independent_decoder_recovers_the_original_payload() {
        let expected = "https://honestqrcode.com/independent-decoder-check";
        let spec = QrSpec {
            data: QrData::Text {
                value: expected.to_owned(),
            },
            render: RenderOptions {
                width: 768,
                error_correction: ErrorCorrection::High,
                ..RenderOptions::default()
            },
        };
        let artifact = render(&spec).expect("PNG render");
        let image = image::load_from_memory_with_format(&artifact.bytes, ImageFormat::Png)
            .expect("PNG image")
            .to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(image);
        let grids = prepared.detect_grids();
        assert_eq!(grids.len(), 1);
        let (_, decoded) = grids[0].decode().expect("independent decode");
        assert_eq!(decoded, expected);
    }

    #[test]
    fn svg_has_quiet_zone_and_no_scripts() {
        let artifact = render(&text_spec(QrFormat::Svg)).expect("SVG render");
        let svg = String::from_utf8(artifact.bytes).expect("UTF-8 SVG");
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("shape-rendering=\"crispEdges\""));
        assert!(!svg.contains("<script"));
    }

    #[test]
    fn wifi_payload_escapes_delimiters() {
        let spec = QrSpec {
            data: QrData::Wifi {
                ssid: "Office;Guest".to_owned(),
                password: Some("pa:ss,word".to_owned()),
                security: WifiSecurity::Wpa,
                hidden: true,
            },
            render: RenderOptions {
                format: QrFormat::Matrix,
                ..RenderOptions::default()
            },
        };
        let payload = payload_bytes(&spec.data).expect("WiFi payload");
        assert_eq!(
            String::from_utf8(payload).expect("UTF-8"),
            "WIFI:T:WPA;S:Office\\;Guest;P:pa\\:ss\\,word;H:true;;"
        );
    }

    #[test]
    fn rejects_same_colors_and_oversized_width() {
        let mut spec = text_spec(QrFormat::Png);
        spec.render.background = "#000000".to_owned();
        assert!(matches!(render(&spec), Err(QrError::InvalidField { .. })));

        spec.render.background = "#ffffff".to_owned();
        spec.render.width = MAX_WIDTH + 1;
        assert!(matches!(render(&spec), Err(QrError::InvalidWidth)));
    }

    #[test]
    fn event_is_a_complete_calendar() {
        let data = QrData::Event {
            title: "Launch".to_owned(),
            start: "2026-07-22T12:00:00Z".to_owned(),
            end: Some("2026-07-22T13:00:00Z".to_owned()),
            location: Some("Online".to_owned()),
            description: None,
        };
        let payload =
            String::from_utf8(payload_bytes(&data).expect("event payload")).expect("UTF-8");
        assert!(payload.starts_with("BEGIN:VCALENDAR\r\nVERSION:2.0"));
        assert!(payload.contains("DTSTART:20260722T120000Z"));
        assert!(payload.ends_with("END:VEVENT\r\nEND:VCALENDAR"));
    }
}
