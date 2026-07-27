//! Deterministic QR payload construction and rendering.
//!
//! The public interface is deliberately small: callers provide a [`QrSpec`] to
//! [`render`] and receive a [`QrArtifact`]. Validation, payload encoding,
//! symbol sizing, rasterization, vector rendering, and hashing stay inside this
//! module so every transport adapter behaves identically.

use std::fmt::Write as FmtWrite;
use std::io::Cursor;

use image::{DynamicImage, GrayImage, ImageFormat, Rgba};
use qrcode::{EcLevel, QrCode, types::Color as ModuleColor};
use serde::de::Error as DeError;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::{Url, form_urlencoded};
use utoipa::ToSchema;

pub const MAX_PAYLOAD_BYTES: usize = 2_953;
pub const MIN_WIDTH: u32 = 64;
pub const MAX_WIDTH: u32 = 4_096;
pub const MAX_MARGIN: u8 = 32;
/// Maximum base64 input length that can decode to [`MAX_PAYLOAD_BYTES`].
const MAX_BASE64_ENCODED_BYTES: usize = MAX_PAYLOAD_BYTES.div_ceil(3) * 4;

/// Requested output width in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Width(u32);

/// Quiet-zone margin in modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Margin(u8);

impl Width {
    const fn new_unchecked(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Margin {
    const fn new_unchecked(value: u8) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u32> for Width {
    type Error = QrError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if (MIN_WIDTH..=MAX_WIDTH).contains(&value) {
            Ok(Self(value))
        } else {
            Err(QrError::InvalidWidth)
        }
    }
}

impl TryFrom<u8> for Margin {
    type Error = QrError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value <= MAX_MARGIN {
            Ok(Self(value))
        } else {
            Err(QrError::InvalidMargin)
        }
    }
}

impl Serialize for Width {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl Serialize for Margin {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Width {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Width::try_from(u32::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl<'de> Deserialize<'de> for Margin {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Margin::try_from(u8::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QrSpec {
    pub data: QrData,
    #[serde(default)]
    pub render: RenderOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
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
#[non_exhaustive]
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
    #[schema(value_type = u32)]
    pub width: Width,
    #[serde(default = "default_margin")]
    #[schema(value_type = u8)]
    pub margin: Margin,
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

const fn default_width() -> Width {
    Width::new_unchecked(512)
}

const fn default_margin() -> Margin {
    Margin::new_unchecked(4)
}

fn default_foreground() -> String {
    "#000000".to_owned()
}

fn default_background() -> String {
    "#ffffff".to_owned()
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
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
#[non_exhaustive]
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

#[derive(Debug, Clone, ToSchema)]
#[schema(as = QrMetadataSchema)]
pub struct QrMetadata {
    pub format: QrFormat,
    pub width: u32,
    pub height: u32,
    pub modules: u32,
    pub version: u8,
    pub payload_bytes: usize,
    pub sha256: String,
}

#[derive(Serialize, ToSchema)]
#[allow(dead_code)]
struct QrMetadataSchema {
    content_type: String,
    extension: String,
    width: u32,
    height: u32,
    modules: u32,
    version: u8,
    payload_bytes: usize,
    sha256: String,
}

impl QrMetadata {
    pub const fn content_type(&self) -> &'static str {
        self.format.content_type()
    }

    pub const fn extension(&self) -> &'static str {
        self.format.extension()
    }
}

impl Serialize for QrMetadata {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("QrMetadata", 8)?;
        state.serialize_field("content_type", self.content_type())?;
        state.serialize_field("extension", self.extension())?;
        state.serialize_field("width", &self.width)?;
        state.serialize_field("height", &self.height)?;
        state.serialize_field("modules", &self.modules)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("payload_bytes", &self.payload_bytes)?;
        state.serialize_field("sha256", &self.sha256)?;
        state.end()
    }
}

#[derive(Debug, Clone)]
pub struct ValidatedQrSpec {
    payload: Vec<u8>,
    render: ValidatedRenderOptions,
}

#[derive(Debug, Clone)]
struct ValidatedRenderOptions {
    format: QrFormat,
    width: Width,
    margin: Margin,
    error_correction: ErrorCorrection,
    foreground: String,
    background: String,
    foreground_rgba: Rgba<u8>,
    background_rgba: Rgba<u8>,
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

impl QrSpec {
    /// Validate the specification and precompute the encoded payload.
    pub fn validate(&self) -> Result<ValidatedQrSpec, QrError> {
        validate_render_options(&self.render)?;
        let payload = payload_bytes(&self.data)?;
        if payload.is_empty() {
            return Err(QrError::EmptyPayload);
        }
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(QrError::PayloadTooLarge {
                actual: payload.len(),
                maximum: MAX_PAYLOAD_BYTES,
            });
        }

        let foreground = parse_color("foreground", &self.render.foreground)?;
        let background = parse_color("background", &self.render.background)?;
        if foreground == background {
            return Err(QrError::InvalidField {
                field: "colors",
                reason: "foreground and background must differ".to_owned(),
            });
        }

        Ok(ValidatedQrSpec {
            payload,
            render: ValidatedRenderOptions {
                format: self.render.format,
                width: self.render.width,
                margin: self.render.margin,
                error_correction: self.render.error_correction,
                foreground: self.render.foreground.clone(),
                background: self.render.background.clone(),
                foreground_rgba: foreground,
                background_rgba: background,
            },
        })
    }
}

/// Render a QR artifact from a validated specification.
pub fn render_validated(spec: &ValidatedQrSpec) -> Result<QrArtifact, QrError> {
    let code =
        QrCode::with_error_correction_level(&spec.payload, spec.render.error_correction.into())
            .map_err(|_| QrError::DataOverflow)?;
    let module_count = u32::try_from(code.width()).map_err(|_| QrError::DataOverflow)?;
    let total_modules = module_count + (u32::from(spec.render.margin.get()) * 2);
    let scale = spec.render.width.get() / total_modules;
    if scale == 0 {
        return Err(QrError::WidthTooSmall);
    }
    let actual_width = total_modules * scale;

    let bytes = match spec.render.format {
        QrFormat::Png => render_png(
            &code,
            module_count,
            actual_width,
            scale,
            spec.render.margin,
            spec.render.foreground_rgba,
            spec.render.background_rgba,
        )?,
        QrFormat::Svg => render_svg(
            &code,
            module_count,
            actual_width,
            spec.render.margin,
            &spec.render.foreground,
            &spec.render.background,
        )?
        .into_bytes(),
        QrFormat::Matrix => render_matrix(&code, module_count)?,
    };

    let sha256 = hex::encode(Sha256::digest(&bytes));
    let version = u8::try_from((module_count.saturating_sub(17)) / 4)
        .map_err(|_| QrError::RenderFailed { format: "metadata" })?;

    Ok(QrArtifact {
        bytes,
        metadata: QrMetadata {
            format: spec.render.format,
            width: actual_width,
            height: actual_width,
            modules: module_count,
            version,
            payload_bytes: spec.payload.len(),
            sha256,
        },
    })
}

/// Render a QR artifact from a complete, transport-independent specification.
pub fn render(spec: &QrSpec) -> Result<QrArtifact, QrError> {
    render_validated(&spec.validate()?)
}

fn validate_render_options(options: &RenderOptions) -> Result<(), QrError> {
    Width::try_from(options.width.get()).map_err(|_| QrError::InvalidWidth)?;
    Margin::try_from(options.margin.get()).map_err(|_| QrError::InvalidMargin)?;
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
        QrData::Bytes { base64 } => decode_bounded_base64(base64)?,
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
            let to = validate_mailbox(to)?;
            let to = percent_encode_mailbox(to);
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

            let mut lines = Vec::with_capacity(12);
            lines.push("BEGIN:VCARD".to_owned());
            lines.push("VERSION:3.0".to_owned());
            lines.push(format!(
                "N:{};{};;;",
                escape_vcard(last_name),
                escape_vcard(first_name)
            ));
            lines.push(format!(
                "FN:{}",
                escape_vcard(format!("{first_name} {last_name}").trim())
            ));
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
            let start = CalendarValue::parse("start", start)?;
            let end = nonempty(end.as_deref())
                .map(|value| CalendarValue::parse("end", value))
                .transpose()?;
            if let Some(end) = end {
                if !start.is_comparable_with(end) {
                    return Err(QrError::InvalidField {
                        field: "end",
                        reason: "must use the same value type and UTC/floating form as start"
                            .to_owned(),
                    });
                }
                if end <= start {
                    return Err(QrError::InvalidField {
                        field: "end",
                        reason: "must be after start".to_owned(),
                    });
                }
            }
            let mut event_lines = vec![
                format!("SUMMARY:{}", escape_vcard(title)),
                start.content_line("DTSTART"),
            ];
            if let Some(end) = end {
                event_lines.push(end.content_line("DTEND"));
            }
            push_vcard(&mut event_lines, "LOCATION", location.as_deref());
            push_vcard(&mut event_lines, "DESCRIPTION", description.as_deref());

            let uid = deterministic_event_uid(&event_lines);
            let mut lines = vec![
                "BEGIN:VCALENDAR".to_owned(),
                "VERSION:2.0".to_owned(),
                "PRODID:-//Honest QR Code//API//EN".to_owned(),
                "BEGIN:VEVENT".to_owned(),
                format!("UID:{uid}"),
                "DTSTAMP:19700101T000000Z".to_owned(),
            ];
            lines.extend(event_lines);
            lines.push("END:VEVENT".to_owned());
            lines.push("END:VCALENDAR".to_owned());
            serialize_content_lines(&lines).into_bytes()
        }
    };
    Ok(payload)
}

fn render_png(
    code: &QrCode,
    module_count: u32,
    actual_width: u32,
    scale: u32,
    margin: Margin,
    foreground: Rgba<u8>,
    background: Rgba<u8>,
) -> Result<Vec<u8>, QrError> {
    let total_modules = module_count + (u32::from(margin.get()) * 2);
    let margin_modules = u32::from(margin.get());
    let foreground_luma = luma_from_rgba(foreground);
    let background_luma = luma_from_rgba(background);
    let mut image = GrayImage::from_pixel(total_modules, total_modules, background_luma);
    let colors = code.to_colors();

    for y in 0..module_count {
        for x in 0..module_count {
            let index = usize::try_from(y * module_count + x)
                .map_err(|_| QrError::RenderFailed { format: "PNG" })?;
            if colors[index] != ModuleColor::Dark {
                continue;
            }
            image.put_pixel(
                x + margin_modules,
                y + margin_modules,
                foreground_luma,
            );
        }
    }

    let image = if scale == 1 {
        image
    } else {
        image::imageops::resize(
            &image,
            actual_width,
            actual_width,
            image::imageops::FilterType::Nearest,
        )
    };

    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(image)
        .write_to(&mut output, ImageFormat::Png)
        .map_err(|_| QrError::RenderFailed { format: "PNG" })?;
    Ok(output.into_inner())
}

fn luma_from_rgba(color: Rgba<u8>) -> image::Luma<u8> {
    image::Luma([color.0[0]])
}

fn render_svg(
    code: &QrCode,
    module_count: u32,
    actual_width: u32,
    margin: Margin,
    foreground: &str,
    background: &str,
) -> Result<String, QrError> {
    let total = module_count + (u32::from(margin.get()) * 2);
    let colors = code.to_colors();
    let mut path = String::with_capacity(colors.len() * 4);

    for y in 0..module_count {
        let mut x = 0;
        while x < module_count {
            let index = usize::try_from(y * module_count + x)
                .map_err(|_| QrError::RenderFailed { format: "SVG" })?;
            if colors[index] != ModuleColor::Dark {
                x += 1;
                continue;
            }
            let start = x;
            while x < module_count {
                let run_index = usize::try_from(y * module_count + x)
                    .map_err(|_| QrError::RenderFailed { format: "SVG" })?;
                if colors[run_index] != ModuleColor::Dark {
                    break;
                }
                x += 1;
            }
            let run = x - start;
            let _ = write!(
                path,
                "M{} {}h{}v1h-{}z",
                start + u32::from(margin.get()),
                y + u32::from(margin.get()),
                run,
                run
            );
        }
    }

    Ok(format!(
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
    ))
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

fn decode_bounded_base64(encoded: &str) -> Result<Vec<u8>, QrError> {
    if encoded.len() > MAX_BASE64_ENCODED_BYTES {
        return Err(QrError::PayloadTooLarge {
            actual: encoded.len().saturating_mul(3) / 4,
            maximum: MAX_PAYLOAD_BYTES,
        });
    }
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| invalid("base64", error))?;
    if decoded.len() > MAX_PAYLOAD_BYTES {
        return Err(QrError::PayloadTooLarge {
            actual: decoded.len(),
            maximum: MAX_PAYLOAD_BYTES,
        });
    }
    Ok(decoded)
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
    validate_mailbox(value).map(|_| ())
}

fn validate_mailbox(value: &str) -> Result<&str, QrError> {
    let trimmed = value.trim();
    let invalid_email = || QrError::InvalidField {
        field: "email",
        reason: "expected one valid ASCII dot-atom email address".to_owned(),
    };
    if trimmed != value || !trimmed.is_ascii() || trimmed.len() > 254 {
        return Err(invalid_email());
    }

    let mut parts = trimmed.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(invalid_email());
    };
    let valid_local = !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'/'
                        | b'='
                        | b'?'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'{'
                        | b'|'
                        | b'}'
                        | b'~'
                        | b'.'
                )
        });
    let valid_domain = domain.contains('.')
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    if !valid_local || !valid_domain {
        return Err(invalid_email());
    }
    Ok(trimmed)
}

fn percent_encode_mailbox(mailbox: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(mailbox.len());
    for byte in mailbox.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'@') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CalendarDate {
    year: u16,
    month: u8,
    day: u8,
}

impl CalendarDate {
    fn parse(value: &str) -> Option<Self> {
        if !value.is_ascii() {
            return None;
        }
        let digits = match value.as_bytes() {
            [year @ .., b'-', _, _, b'-', _, _] if year.len() == 4 => {
                let mut compact = [0_u8; 8];
                compact[..4].copy_from_slice(year);
                compact[4..6].copy_from_slice(&value.as_bytes()[5..7]);
                compact[6..].copy_from_slice(&value.as_bytes()[8..10]);
                compact
            }
            bytes if bytes.len() == 8 => bytes.try_into().ok()?,
            _ => return None,
        };
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let year = parse_decimal(&digits[..4])?;
        let month = u8::try_from(parse_decimal(&digits[4..6])?).ok()?;
        let day = u8::try_from(parse_decimal(&digits[6..8])?).ok()?;
        let days_in_month = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if is_leap_year(year) => 29,
            2 => 28,
            _ => return None,
        };
        if year == 0 || day == 0 || day > days_in_month {
            return None;
        }
        Some(Self { year, month, day })
    }

    fn compact(self) -> String {
        format!("{:04}{:02}{:02}", self.year, self.month, self.day)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CalendarDateTime {
    date: CalendarDate,
    hour: u8,
    minute: u8,
    second: u8,
    utc: bool,
}

impl CalendarDateTime {
    fn parse(value: &str) -> Option<Self> {
        if !value.is_ascii() {
            return None;
        }
        let (value, utc) = value
            .strip_suffix('Z')
            .map_or((value, false), |value| (value, true));
        let (date, time) = match value.len() {
            15 if value.as_bytes()[8] == b'T' => (&value[..8], &value[9..]),
            19 if value.as_bytes()[10] == b'T' => (&value[..10], &value[11..]),
            _ => return None,
        };
        let date = CalendarDate::parse(date)?;
        let time_digits: [u8; 6] = match time.as_bytes() {
            bytes if bytes.len() == 6 => bytes.try_into().ok()?,
            [hour @ .., b':', _, _, b':', _, _] if hour.len() == 2 => {
                let mut compact = [0_u8; 6];
                compact[..2].copy_from_slice(hour);
                compact[2..4].copy_from_slice(&time.as_bytes()[3..5]);
                compact[4..].copy_from_slice(&time.as_bytes()[6..8]);
                compact
            }
            _ => return None,
        };
        if !time_digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let hour = u8::try_from(parse_decimal(&time_digits[..2])?).ok()?;
        let minute = u8::try_from(parse_decimal(&time_digits[2..4])?).ok()?;
        let second = u8::try_from(parse_decimal(&time_digits[4..6])?).ok()?;
        // RFC 5545's TIME grammar permits seconds 00-60. Determining whether
        // 60 denotes an actual positive leap second requires historical and
        // future timezone/leap-second data, so this parser validates syntax
        // consistently and preserves the supplied value.
        if hour > 23 || minute > 59 || second > 60 {
            return None;
        }
        Some(Self {
            date,
            hour,
            minute,
            second,
            utc,
        })
    }

    fn compact(self) -> String {
        format!(
            "{}T{:02}{:02}{:02}{}",
            self.date.compact(),
            self.hour,
            self.minute,
            self.second,
            if self.utc { "Z" } else { "" }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CalendarValue {
    Date(CalendarDate),
    DateTime(CalendarDateTime),
}

impl CalendarValue {
    fn parse(field: &'static str, value: &str) -> Result<Self, QrError> {
        let value = value.trim();
        CalendarDate::parse(value)
            .map(Self::Date)
            .or_else(|| CalendarDateTime::parse(value).map(Self::DateTime))
            .ok_or_else(|| QrError::InvalidField {
                field,
                reason: concat!(
                    "expected a real date or time in YYYYMMDD, YYYY-MM-DD, ",
                    "YYYYMMDDTHHMMSS[Z], or YYYY-MM-DDTHH:MM:SS[Z] form"
                )
                .to_owned(),
            })
    }

    const fn is_comparable_with(self, other: Self) -> bool {
        match (self, other) {
            (Self::Date(_), Self::Date(_)) => true,
            (Self::DateTime(left), Self::DateTime(right)) => left.utc == right.utc,
            _ => false,
        }
    }

    fn content_line(self, name: &str) -> String {
        match self {
            Self::Date(date) => format!("{name};VALUE=DATE:{}", date.compact()),
            Self::DateTime(date_time) => format!("{name}:{}", date_time.compact()),
        }
    }
}

fn parse_decimal(bytes: &[u8]) -> Option<u16> {
    bytes.iter().try_fold(0_u16, |value, byte| {
        value
            .checked_mul(10)?
            .checked_add(u16::from(byte.checked_sub(b'0')?))
    })
}

const fn is_leap_year(year: u16) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn deterministic_event_uid(content_lines: &[String]) -> String {
    let mut digest = Sha256::new();
    for line in content_lines {
        digest.update(line.len().to_be_bytes());
        digest.update(line.as_bytes());
    }
    format!("{}@honestqrcode.com", hex::encode(digest.finalize()))
}

fn serialize_content_lines(lines: &[String]) -> String {
    lines
        .iter()
        .map(|line| fold_content_line(line))
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n"
}

fn fold_content_line(line: &str) -> String {
    let mut folded = String::with_capacity(line.len() + (line.len() / 74 * 3));
    let mut remainder = line;
    let mut first = true;
    while !remainder.is_empty() {
        let capacity = if first { 75 } else { 74 };
        let mut split = remainder.len().min(capacity);
        while !remainder.is_char_boundary(split) {
            split -= 1;
        }
        if !first {
            folded.push_str("\r\n ");
        }
        folded.push_str(&remainder[..split]);
        remainder = &remainder[split..];
        first = false;
    }
    folded
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
    let mut escaped = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                escaped.push_str("\\n");
            }
            '\n' => escaped.push_str("\\n"),
            '\\' | ';' | ',' => {
                escaped.push('\\');
                escaped.push(character);
            }
            // RFC 5545 TEXT values cannot contain control characters other
            // than horizontal tab. Replace them visibly instead of emitting
            // octets that can truncate or corrupt downstream parsers.
            '\t' => escaped.push('\t'),
            character if character.is_control() => escaped.push('\u{fffd}'),
            character => escaped.push(character),
        }
    }
    escaped
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
                width: Width::try_from(768).expect("valid width"),
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
        assert!(matches!(
            Width::try_from(MAX_WIDTH + 1),
            Err(QrError::InvalidWidth)
        ));
        let invalid_width = serde_json::json!({
            "data": {"kind": "text", "value": "hello"},
            "render": {"format": "png", "width": MAX_WIDTH + 1}
        });
        assert!(serde_json::from_value::<QrSpec>(invalid_width).is_err());
    }

    #[test]
    fn rejects_oversized_base64_before_decoding() {
        let oversized = "A".repeat(MAX_BASE64_ENCODED_BYTES + 1);
        let data = QrData::Bytes { base64: oversized };
        assert!(matches!(
            payload_bytes(&data),
            Err(QrError::PayloadTooLarge { .. })
        ));
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
        assert!(payload.contains("\r\nUID:"));
        assert!(payload.contains("\r\nDTSTAMP:"));
        assert!(payload.ends_with("END:VEVENT\r\nEND:VCALENDAR\r\n"));
    }

    fn event(start: &str, end: Option<&str>) -> QrData {
        QrData::Event {
            title: "Launch".to_owned(),
            start: start.to_owned(),
            end: end.map(str::to_owned),
            location: Some("Online".to_owned()),
            description: Some("Release".to_owned()),
        }
    }

    fn payload_string(data: &QrData) -> Result<String, QrError> {
        payload_bytes(data).map(|payload| String::from_utf8(payload).expect("UTF-8 payload"))
    }

    #[test]
    fn event_rejects_impossible_dates_and_times() {
        for start in [
            "20260229",
            "2026-04-31",
            "2026-00-10",
            "2026-01-00",
            "2026-07-22T24:00:00Z",
            "2026-07-22T12:60:00Z",
            "2026-07-22T12:00:61Z",
            "2026-07-22T12:00:0é",
        ] {
            assert!(
                matches!(
                    payload_bytes(&event(start, None)),
                    Err(QrError::InvalidField { field: "start", .. })
                ),
                "accepted invalid start {start}"
            );
        }

        payload_bytes(&event("2024-02-29", None)).expect("leap day is valid");
    }

    #[test]
    fn event_accepts_and_preserves_rfc_leap_second() {
        let payload = payload_string(&event("19970630T235960Z", Some("19970701T000000Z")))
            .expect("RFC 5545 leap second");

        assert!(payload.contains("DTSTART:19970630T235960Z\r\n"));
        assert!(payload.contains("DTEND:19970701T000000Z\r\n"));

        let floating = payload_string(&event("1997-06-30T18:29:60", Some("1997-06-30T18:30:00")))
            .expect("floating leap-second syntax");
        assert!(floating.contains("DTSTART:19970630T182960\r\n"));
    }

    #[test]
    fn event_rejects_mixed_types_and_end_before_start() {
        for data in [
            event("2026-07-22", Some("2026-07-22T12:00:00Z")),
            event("2026-07-22T12:00:00Z", Some("2026-07-22")),
            event("2026-07-22T12:00:00Z", Some("2026-07-22T13:00:00")),
            event("2026-07-23", Some("2026-07-22")),
            event("2026-07-22", Some("2026-07-22")),
            event("2026-07-22T12:00:00Z", Some("2026-07-22T11:59:59Z")),
            event("2026-07-22T12:00:00Z", Some("2026-07-22T12:00:00Z")),
        ] {
            assert!(matches!(
                payload_bytes(&data),
                Err(QrError::InvalidField { field: "end", .. })
            ));
        }
    }

    #[test]
    fn event_date_values_are_typed_and_equivalent_forms_are_canonical() {
        let compact = payload_string(&event("20260722", Some("20260723"))).expect("compact");
        let extended = payload_string(&event("2026-07-22", Some("2026-07-23"))).expect("extended");

        assert_eq!(compact, extended);
        assert!(compact.contains("DTSTART;VALUE=DATE:20260722"));
        assert!(compact.contains("DTEND;VALUE=DATE:20260723"));
    }

    #[test]
    fn event_datetime_forms_are_canonical_and_output_is_deterministic() {
        let compact =
            payload_string(&event("20260722T120000Z", Some("20260722T130000Z"))).expect("compact");
        let extended = payload_string(&event("2026-07-22T12:00:00Z", Some("2026-07-22T13:00:00Z")))
            .expect("extended");

        assert_eq!(compact, extended);
        assert_eq!(
            compact,
            payload_string(&event("2026-07-22T12:00:00Z", Some("2026-07-22T13:00:00Z")))
                .expect("repeat")
        );
    }

    #[test]
    fn event_content_lines_are_folded_without_splitting_utf8() {
        let data = QrData::Event {
            title: "Launch — ".repeat(20),
            start: "20260722".to_owned(),
            end: None,
            location: None,
            description: None,
        };
        let payload = payload_string(&data).expect("event payload");
        let unfolded = payload.replace("\r\n ", "");

        assert!(payload.contains("\r\n "));
        assert!(unfolded.contains(&format!(
            "SUMMARY:{}\r\n",
            escape_vcard(&"Launch — ".repeat(20))
        )));
        assert!(
            payload
                .split("\r\n")
                .all(|physical_line| physical_line.len() <= 75)
        );
    }

    #[test]
    fn calendar_text_never_emits_forbidden_control_characters() {
        let data = QrData::Event {
            title: "Launch\0\u{7f}\u{85}".to_owned(),
            start: "20260722".to_owned(),
            end: None,
            location: Some("Room\u{1f}".to_owned()),
            description: Some("Line one\r\nLine two\tTabbed".to_owned()),
        };
        let payload = payload_string(&data).expect("sanitized event payload");

        assert!(!payload.chars().any(|character| {
            character.is_control() && !matches!(character, '\r' | '\n' | '\t')
        }));
        assert!(payload.contains("SUMMARY:Launch���\r\n"));
        assert!(payload.contains("LOCATION:Room�\r\n"));
        assert!(payload.contains("DESCRIPTION:Line one\\nLine two\tTabbed\r\n"));
    }

    #[test]
    fn mailto_normal_address_is_preserved() {
        let data = QrData::Email {
            to: "user.name@example.com".to_owned(),
            subject: None,
            body: None,
        };

        assert_eq!(
            payload_string(&data).expect("mailto"),
            "mailto:user.name@example.com"
        );
    }

    #[test]
    fn mailto_address_delimiters_are_encoded_once() {
        let data = QrData::Email {
            to: "sales&support+qr?code%@example.com".to_owned(),
            subject: Some("hello".to_owned()),
            body: None,
        };

        assert_eq!(
            payload_string(&data).expect("mailto"),
            "mailto:sales%26support%2Bqr%3Fcode%25@example.com?subject=hello"
        );
    }

    #[test]
    fn mailto_rejects_recipient_and_query_injection() {
        for address in [
            "first@example.com,second@example.com",
            "first@example.com?subject=injected",
            "first@example.com&to=second@example.com",
            "first@example.com;second@example.com",
            "first@example.com@evil.example",
            ".leading@example.com",
            "trailing.@example.com",
            "double..dot@example.com",
            "user@-example.com",
            "user@example-.com",
        ] {
            let data = QrData::Email {
                to: address.to_owned(),
                subject: None,
                body: None,
            };
            assert!(
                matches!(
                    payload_bytes(&data),
                    Err(QrError::InvalidField { field: "email", .. })
                ),
                "accepted malformed mailbox {address}"
            );
        }
    }
}
