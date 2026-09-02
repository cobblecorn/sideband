//! Minimal PCM16 WAV writer. Patches the two size fields on finish.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

pub struct WavWriter {
    inner: BufWriter<File>,
    data_bytes: u32,
}

impl WavWriter {
    pub fn create(path: &Path, sample_rate: u32, channels: u16, bits: u16) -> std::io::Result<Self> {
        let block_align: u16 = channels * bits / 8;
        let byte_rate: u32 = sample_rate * block_align as u32;

        let mut w = BufWriter::new(File::create(path)?);
        w.write_all(b"RIFF")?;
        w.write_all(&0u32.to_le_bytes())?; // patched in finish()
        w.write_all(b"WAVE")?;
        w.write_all(b"fmt ")?;
        w.write_all(&16u32.to_le_bytes())?;
        w.write_all(&1u16.to_le_bytes())?; // WAVE_FORMAT_PCM
        w.write_all(&channels.to_le_bytes())?;
        w.write_all(&sample_rate.to_le_bytes())?;
        w.write_all(&byte_rate.to_le_bytes())?;
        w.write_all(&block_align.to_le_bytes())?;
        w.write_all(&bits.to_le_bytes())?;
        w.write_all(b"data")?;
        w.write_all(&0u32.to_le_bytes())?; // patched in finish()

        Ok(Self { inner: w, data_bytes: 0 })
    }

    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(bytes)?;
        self.data_bytes += bytes.len() as u32;
        Ok(())
    }

    /// Writes `count` zero bytes. Used to fill the gaps where the target
    /// process produced no audio at all, see the note in loopback.rs.
    pub fn write_silence(&mut self, count: usize) -> std::io::Result<()> {
        const CHUNK: [u8; 4096] = [0u8; 4096];
        let mut left = count;
        while left > 0 {
            let n = left.min(CHUNK.len());
            self.write(&CHUNK[..n])?;
            left -= n;
        }
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<u32> {
        self.inner.flush()?;
        let mut f = self
            .inner
            .into_inner()
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(36 + self.data_bytes).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&self.data_bytes.to_le_bytes())?;
        f.flush()?;

        Ok(self.data_bytes)
    }
}
