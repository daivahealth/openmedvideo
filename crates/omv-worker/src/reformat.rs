//! Multiplanar reformats for CT stacks (design §4.2 / Phase 3 item).
//!
//! The axial pass already fetches every windowed slice as an 8-bit PNG;
//! stacking those gives a voxel volume that can be resliced without touching
//! DICOM pixel data again:
//!   - coronal:  fix an image row y   → frame is (width × n_slices)
//!   - sagittal: fix an image column x → frame is (height × n_slices)
//! Frames are emitted with the most superior slice at the top (the stack is
//! already in ascending geometric order when this runs). The z axis is
//! stretched by slice-spacing / pixel-spacing at encode time so anatomy
//! keeps its aspect.
//!
//! Reformats only run when the geometry is trustworthy: geometric slice
//! ordering succeeded, enough slices exist, and no PHI mask applies (a
//! masked band would streak through every reformatted frame).

use anyhow::{bail, Context, Result};

pub struct Volume {
    pub width: usize,
    pub height: usize,
    slices: Vec<Vec<u8>>,
}

impl Volume {
    pub fn new() -> Self {
        Self { width: 0, height: 0, slices: Vec::new() }
    }

    pub fn n_slices(&self) -> usize {
        self.slices.len()
    }

    /// Decodes one axial PNG (as served by Orthanc's renderer) into 8-bit
    /// grayscale and appends it. All slices must share dimensions.
    pub fn push_png(&mut self, data: &[u8]) -> Result<()> {
        let decoder = png::Decoder::new(std::io::Cursor::new(data));
        let mut reader = decoder.read_info().context("reading PNG header")?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).context("decoding PNG frame")?;
        let (w, h) = (info.width as usize, info.height as usize);

        let gray: Vec<u8> = match (info.color_type, info.bit_depth) {
            (png::ColorType::Grayscale, png::BitDepth::Eight) => {
                buf.truncate(w * h);
                buf
            }
            (png::ColorType::Grayscale, png::BitDepth::Sixteen) => {
                // Big-endian 16-bit; keep the high byte.
                buf.chunks_exact(2).take(w * h).map(|c| c[0]).collect()
            }
            (png::ColorType::Rgb, png::BitDepth::Eight) => {
                buf.chunks_exact(3).take(w * h).map(luma).collect()
            }
            (png::ColorType::Rgba, png::BitDepth::Eight) => {
                buf.chunks_exact(4).take(w * h).map(luma).collect()
            }
            (ct, bd) => bail!("unsupported PNG format for reformat: {ct:?}/{bd:?}"),
        };

        if self.slices.is_empty() {
            self.width = w;
            self.height = h;
        } else if w != self.width || h != self.height {
            bail!("slice dimensions changed mid-series ({w}x{h} vs {}x{})",
                  self.width, self.height);
        }
        self.slices.push(gray);
        Ok(())
    }

    /// Coronal frames: one per image row y, each (width × n_slices),
    /// superior (last slice) at the top.
    pub fn coronal(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        let (w, z) = (self.width, self.n_slices());
        (0..self.height).map(move |y| {
            let mut frame = vec![0u8; w * z];
            for (row, slice_idx) in (0..z).rev().enumerate() {
                let src = &self.slices[slice_idx][y * w..(y + 1) * w];
                frame[row * w..(row + 1) * w].copy_from_slice(src);
            }
            frame
        })
    }

    /// Sagittal frames: one per image column x, each (height × n_slices),
    /// superior at the top.
    pub fn sagittal(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        let (w, h, z) = (self.width, self.height, self.n_slices());
        (0..w).map(move |x| {
            let mut frame = vec![0u8; h * z];
            for (row, slice_idx) in (0..z).rev().enumerate() {
                for y in 0..h {
                    frame[row * h + y] = self.slices[slice_idx][y * w + x];
                }
            }
            frame
        })
    }
}

fn luma(px: &[u8]) -> u8 {
    ((px[0] as u32 * 299 + px[1] as u32 * 587 + px[2] as u32 * 114) / 1000) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a 2x2xN volume from raw slices via in-memory PNG encoding.
    fn volume(slices: &[[u8; 4]]) -> Volume {
        let mut v = Volume::new();
        for s in slices {
            let mut png_bytes = Vec::new();
            {
                let mut enc = png::Encoder::new(&mut png_bytes, 2, 2);
                enc.set_color(png::ColorType::Grayscale);
                enc.set_depth(png::BitDepth::Eight);
                enc.write_header().unwrap().write_image_data(s).unwrap();
            }
            v.push_png(&png_bytes).unwrap();
        }
        v
    }

    #[test]
    fn reslicing_geometry() {
        // Slice values encode (slice, y, x) so provenance is checkable.
        // slice 0: [ 0  1 ]   slice 1: [10 11]   slice 2: [20 21]
        //          [ 2  3 ]            [12 13]            [22 23]
        let v = volume(&[[0, 1, 2, 3], [10, 11, 12, 13], [20, 21, 22, 23]]);
        assert_eq!((v.width, v.height, v.n_slices()), (2, 2, 3));

        // Coronal y=0: rows are slices (superior=slice2 first), cols are x.
        let cor: Vec<Vec<u8>> = v.coronal().collect();
        assert_eq!(cor.len(), 2, "one frame per row");
        assert_eq!(cor[0], vec![20, 21, 10, 11, 0, 1]);
        assert_eq!(cor[1], vec![22, 23, 12, 13, 2, 3]);

        // Sagittal x=0: rows are slices (superior first), cols are y.
        let sag: Vec<Vec<u8>> = v.sagittal().collect();
        assert_eq!(sag.len(), 2, "one frame per column");
        assert_eq!(sag[0], vec![20, 22, 10, 12, 0, 2]);
        assert_eq!(sag[1], vec![21, 23, 11, 13, 1, 3]);
    }

    #[test]
    fn dimension_change_rejected() {
        let mut v = volume(&[[0, 1, 2, 3]]);
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 3, 1);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&[1, 2, 3]).unwrap();
        }
        assert!(v.push_png(&png_bytes).is_err());
    }
}
