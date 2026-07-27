use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use honestqr_core::{
    ErrorCorrection, Margin, QrData, QrFormat, QrSpec, RenderOptions, Width, render,
};

#[derive(Debug, Parser)]
#[command(
    name = "honestqr",
    version,
    about = "Generate QR codes without a server"
)]
struct Args {
    /// Text to encode. Ignored when --spec is supplied.
    data: Option<String>,
    /// Read a complete QrSpec JSON document from this file.
    #[arg(long)]
    spec: Option<PathBuf>,
    /// Output file, or - for stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = FormatArg::Png)]
    format: FormatArg,
    #[arg(long, default_value_t = 512)]
    width: u32,
    #[arg(long, default_value_t = 4)]
    margin: u8,
    #[arg(long, value_enum, default_value_t = EccArg::Medium)]
    error_correction: EccArg,
    #[arg(long, default_value = "#000000")]
    foreground: String,
    #[arg(long, default_value = "#ffffff")]
    background: String,
    /// Emit artifact metadata as JSON on stderr.
    #[arg(long)]
    metadata: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FormatArg {
    Png,
    Svg,
    Matrix,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EccArg {
    Low,
    Medium,
    Quartile,
    High,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let spec = if let Some(path) = &args.spec {
        serde_json::from_slice::<QrSpec>(&std::fs::read(path)?)?
    } else {
        let data = args.data.ok_or("provide text or --spec <file>")?;
        QrSpec {
            data: QrData::Text { value: data },
            render: RenderOptions {
                format: args.format.into(),
                width: Width::try_from(args.width)?,
                margin: Margin::try_from(args.margin)?,
                error_correction: args.error_correction.into(),
                foreground: args.foreground,
                background: args.background,
            },
        }
    };
    let artifact = render(&spec)?;

    let output = args
        .output
        .unwrap_or_else(|| PathBuf::from(format!("qr.{}", artifact.metadata.extension())));
    if output.as_os_str() == "-" {
        std::io::stdout().lock().write_all(&artifact.bytes)?;
    } else {
        std::fs::write(&output, &artifact.bytes)?;
    }

    if args.metadata {
        eprintln!("{}", serde_json::to_string(&artifact.metadata)?);
    }
    Ok(())
}

impl From<FormatArg> for QrFormat {
    fn from(value: FormatArg) -> Self {
        match value {
            FormatArg::Png => Self::Png,
            FormatArg::Svg => Self::Svg,
            FormatArg::Matrix => Self::Matrix,
        }
    }
}

impl From<EccArg> for ErrorCorrection {
    fn from(value: EccArg) -> Self {
        match value {
            EccArg::Low => Self::Low,
            EccArg::Medium => Self::Medium,
            EccArg::Quartile => Self::Quartile,
            EccArg::High => Self::High,
        }
    }
}
