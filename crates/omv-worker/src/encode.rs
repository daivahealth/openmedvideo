//! ffmpeg wrapper: PNG frames in via stdin, HLS (fMP4/CMAF) out to a local
//! directory that the caller uploads to object storage afterwards.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};

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
    pub fn start(out_dir: &Path, fps: f64, all_intra: bool) -> Result<Self> {
        let gop = if all_intra { 1 } else { fps.round().max(1.0) as i64 };
        let seg = out_dir.join("seg_%05d.m4s");
        let playlist = out_dir.join("index.m3u8");

        let mut child = Command::new("ffmpeg")
            .args(["-y", "-hide_banner", "-loglevel", "error"])
            .args(["-f", "image2pipe", "-framerate", &fps.to_string(), "-i", "pipe:0"])
            .arg("-an")
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "18"])
            // Medical grayscale bands easily at low bitrates; CRF 18 is the
            // quality-targeted setting from design §4.3.
            .args(["-pix_fmt", "yuv420p"])
            // yuv420p requires even dimensions; some detectors/US crops are odd.
            .args(["-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2"])
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
