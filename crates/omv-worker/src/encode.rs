//! ffmpeg wrapper: PNG frames in via stdin, HLS (fMP4/CMAF) out to a local
//! directory that the caller uploads to object storage afterwards.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tracing::info;

/// Video encoder backend (design §4.3): NVENC on GPU hosts is a 10-20x
/// throughput win; libx264 is the universal software fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoder {
    Nvenc,
    X264,
}

impl Encoder {
    pub fn name(self) -> &'static str {
        match self {
            Self::Nvenc => "h264_nvenc",
            Self::X264 => "libx264",
        }
    }

    /// Codec + quality arguments. `quality` is CRF for x264 and CQ for
    /// NVENC — the same "smaller is better" dial in both worlds.
    pub fn quality_args(self, quality: u8) -> Vec<String> {
        let q = quality.to_string();
        match self {
            Self::X264 => vec![
                "-c:v".into(), "libx264".into(),
                "-preset".into(), "veryfast".into(),
                "-crf".into(), q,
            ],
            Self::Nvenc => vec![
                "-c:v".into(), "h264_nvenc".into(),
                "-preset".into(), "p5".into(),
                "-tune".into(), "hq".into(),
                "-rc".into(), "vbr".into(),
                "-cq".into(), q,
                "-b:v".into(), "0".into(),
            ],
        }
    }
}

/// Resolves the encoder from the OMV_ENCODER preference:
///   "auto" (default) — use NVENC when a working GPU session opens, else x264
///   "nvenc"          — require NVENC; fail fast so a misconfigured GPU host
///                      surfaces in ops instead of silently encoding 10x slower
///   "x264"           — force software
///
/// Detection is a real encode smoke-test, not a check of ffmpeg's compiled-in
/// encoder list: h264_nvenc is often present in the build but unusable
/// without an NVIDIA device/driver.
pub async fn detect_encoder(pref: &str) -> Result<Encoder> {
    let enc = match pref {
        "x264" => Encoder::X264,
        "nvenc" => {
            if !nvenc_works().await {
                bail!("OMV_ENCODER=nvenc but h264_nvenc cannot initialize \
                       (no NVIDIA device/driver visible to ffmpeg?)");
            }
            Encoder::Nvenc
        }
        _ => {
            if nvenc_works().await {
                Encoder::Nvenc
            } else {
                info!("h264_nvenc unavailable, using libx264");
                Encoder::X264
            }
        }
    };
    info!(encoder = enc.name(), "video encoder selected");
    Ok(enc)
}

async fn nvenc_works() -> bool {
    Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i", "color=black:size=64x64:rate=1"])
        .args(["-frames:v", "1", "-c:v", "h264_nvenc", "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

pub struct HlsEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
}

impl HlsEncoder {
    /// Starts ffmpeg producing VOD HLS into `out_dir`.
    ///
    /// `all_intra` is used for CT/MRI stacks (design D6): every frame is a
    /// keyframe so players can frame-step and seek instantly. Cine content
    /// uses a ~1s GOP instead.
    /// Starts ffmpeg consuming raw 8-bit grayscale frames of a fixed size
    /// (used by the multiplanar reformats), scaling the output to
    /// `out_w`×`out_h` (e.g. to stretch the slice axis to true aspect).
    pub fn start_raw(
        out_dir: &Path,
        fps: f64,
        encoder: Encoder,
        in_size: (usize, usize),
        out_size: (usize, usize),
    ) -> Result<Self> {
        let seg = out_dir.join("seg_%05d.m4s");
        let playlist = out_dir.join("index.m3u8");
        // Reformats are stacks: all-intra for frame stepping (design D6).
        let vf = format!("scale={}:{}", out_size.0 & !1, out_size.1 & !1);
        let mut child = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "rawvideo", "-pixel_format", "gray"])
            .args(["-video_size", &format!("{}x{}", in_size.0, in_size.1)])
            .args(["-framerate", &fps.to_string(), "-i", "pipe:0"])
            .arg("-an")
            .args(encoder.quality_args(18))
            .args(["-pix_fmt", "yuv420p"])
            .args(["-vf", &vf])
            .args(["-g", "1", "-keyint_min", "1", "-sc_threshold", "0"])
            .args(["-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod"])
            .args(["-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "init.mp4"])
            .args(["-hls_segment_filename", seg.to_str().context("segment path")?])
            .arg(playlist.to_str().context("playlist path")?)
            .stdin(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawning ffmpeg — is it installed and on PATH?")?;
        let stdin = child.stdin.take();
        Ok(Self { child, stdin })
    }

    pub fn start(
        out_dir: &Path,
        fps: f64,
        all_intra: bool,
        encoder: Encoder,
        phi_filter: Option<&str>,
    ) -> Result<Self> {
        let gop = if all_intra { 1 } else { fps.round().max(1.0) as i64 };
        let seg = out_dir.join("seg_%05d.m4s");
        let playlist = out_dir.join("index.m3u8");
        // PHI masks/crops (design §7.2) run BEFORE anything else so stripped
        // regions never reach the encoder in any form.
        let vf = match phi_filter {
            Some(f) => format!("{f},scale=trunc(iw/2)*2:trunc(ih/2)*2"),
            None => "scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string(),
        };

        let mut child = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "image2pipe", "-framerate", &fps.to_string(), "-i", "pipe:0"])
            .arg("-an")
            // Medical grayscale bands easily at low bitrates; quality 18 is
            // the quality-targeted setting from design §4.3.
            .args(encoder.quality_args(18))
            .args(["-pix_fmt", "yuv420p"])
            // yuv420p requires even dimensions; some detectors/US crops are odd.
            .args(["-vf", &vf])
            .args(["-g", &gop.to_string(), "-keyint_min", &gop.to_string()])
            .args(["-sc_threshold", "0"])
            .args(["-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod"])
            .args(["-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "init.mp4"])
            .args(["-hls_segment_filename", seg.to_str().context("segment path")?])
            .arg(playlist.to_str().context("playlist path")?)
            .stdin(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawning ffmpeg — is it installed and on PATH?")?;

        let stdin = child.stdin.take();
        Ok(Self { child, stdin })
    }

    pub async fn write_frame(&mut self, png: &[u8]) -> Result<()> {
        self.stdin
            .as_mut()
            .context("encoder stdin already closed")?
            .write_all(png)
            .await
            .context("writing frame to ffmpeg")
    }

    /// Closes stdin and waits for ffmpeg to finalize the playlist.
    pub async fn finish(mut self) -> Result<()> {
        drop(self.stdin.take()); // EOF signals end of input
        let status = self.child.wait().await?;
        if !status.success() {
            bail!("ffmpeg exited with {status}");
        }
        Ok(())
    }
}

/// The rare share/export case (design §7.2) must survive WhatsApp's ~16 MB
/// re-compression window; stay comfortably under it.
const EXPORT_SIZE_LIMIT: u64 = 14 * 1024 * 1024;

/// Produces `export.mp4` in `out_dir` from the HLS output already there.
///
/// First pass is a lossless re-mux (`-c copy`) of the segments — instant and
/// identical quality. If that lands over the size limit (long stacks, big
/// matrices), it re-encodes with a normal GOP and higher CRF: an export is
/// watched, not frame-stepped, so all-intra isn't needed there.
pub async fn mux_export(out_dir: &Path, encoder: Encoder) -> Result<std::path::PathBuf> {
    let playlist = out_dir.join("index.m3u8");
    let export = out_dir.join("export.mp4");

    let run = |args: Vec<String>| async move {
        let status = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error"])
            .args(args)
            .status()
            .await
            .context("spawning ffmpeg for export")?;
        if !status.success() {
            bail!("ffmpeg export exited with {status}");
        }
        Ok::<_, anyhow::Error>(())
    };

    let p = playlist.to_str().context("playlist path")?.to_string();
    let e = export.to_str().context("export path")?.to_string();
    run(vec![
        "-i".into(), p.clone(),
        "-c".into(), "copy".into(),
        "-movflags".into(), "+faststart".into(),
        e.clone(),
    ])
    .await?;

    if std::fs::metadata(&export)?.len() > EXPORT_SIZE_LIMIT {
        let mut args = vec!["-i".into(), p];
        args.extend(encoder.quality_args(26));
        args.extend([
            "-pix_fmt".into(), "yuv420p".into(),
            "-movflags".into(), "+faststart".into(),
            e,
        ]);
        run(args).await?;
    }
    Ok(export)
}

/// Extracts a poster JPEG from the already-encoded output (never from the
/// raw source): the poster must inherit the PHI stripping and windowing that
/// went into the video (design §7.2). Reads export.mp4 rather than the HLS
/// playlist — seeking a local fMP4 playlist silently produces no frame
/// (ffmpeg exits 0 with a "partial file" warning), while the faststart MP4
/// seeks reliably. Requires mux_export() to have run first.
pub async fn extract_poster(out_dir: &Path, at_secs: f64) -> Result<Vec<u8>> {
    let source = out_dir.join("export.mp4");
    let poster = out_dir.join("__poster.jpg");
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-ss", &format!("{at_secs:.3}")])
        .args(["-i", source.to_str().context("export path")?])
        .args(["-frames:v", "1", "-q:v", "3"])
        .arg(poster.to_str().context("poster path")?)
        .status()
        .await
        .context("spawning ffmpeg for poster")?;
    if !status.success() {
        bail!("ffmpeg poster extraction exited with {status}");
    }
    let bytes = std::fs::read(&poster)
        .context("poster frame not produced (seek past end of video?)")?;
    std::fs::remove_file(&poster).ok(); // keep it out of the rendition upload
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_args_per_encoder() {
        let x = Encoder::X264.quality_args(18);
        assert!(x.contains(&"libx264".to_string()) && x.contains(&"-crf".to_string()));
        let n = Encoder::Nvenc.quality_args(19);
        assert!(n.contains(&"h264_nvenc".to_string()));
        assert!(n.contains(&"-cq".to_string()) && n.contains(&"19".to_string()));
        assert!(!n.contains(&"-crf".to_string()), "CRF is an x264 dial, not NVENC's");
    }

    #[tokio::test]
    async fn forced_nvenc_fails_fast_without_gpu() {
        // On hosts without an NVIDIA device (this CI/dev box), forcing nvenc
        // must be a hard error, and auto must fall back to x264.
        if nvenc_works().await {
            return; // actually on a GPU box — nothing to assert here
        }
        assert!(detect_encoder("nvenc").await.is_err());
        assert_eq!(detect_encoder("auto").await.unwrap(), Encoder::X264);
        assert_eq!(detect_encoder("x264").await.unwrap(), Encoder::X264);
    }
}
