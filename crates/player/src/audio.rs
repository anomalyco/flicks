use crate::{Error, Result};
use flicks_audio::{Channel, layout};
use flicks_container::matroska::Codec;

pub(crate) enum Decoder {
    Aac(flicks_aac::Decoder),
    Ac3(Box<flicks_ac3::Decoder>),
    Opus(flicks_opus::Decoder),
}

pub(crate) struct Chunk {
    pub pts: f64,
    pub rate: u32,
    pub layout: &'static [Channel],
    pub samples: Vec<f32>,
}

impl Chunk {
    pub fn end(&self) -> f64 {
        self.pts + self.samples.len() as f64 / self.layout.len() as f64 / self.rate as f64
    }
    pub fn trim(&mut self, start: f64, end: f64) {
        let channels = self.layout.len();
        let keep = ((end - self.pts).max(0.0) * self.rate as f64).round() as usize;
        self.samples.truncate(keep.saturating_mul(channels));
        let skip =
            (((start - self.pts).max(0.0) * self.rate as f64).ceil() as usize).min(self.samples.len() / channels);
        self.samples.drain(..skip * channels);
        self.pts += skip as f64 / self.rate as f64;
    }
}

impl Decoder {
    pub fn new(codec: &Codec) -> Result<Self> {
        Ok(match codec {
            Codec::Aac { asc } => Self::Aac(flicks_aac::Decoder::new(asc).map_err(Error::decode)?),
            Codec::Ac3 | Codec::Eac3 => {
                let mut decoder = flicks_ac3::Decoder::new();
                decoder.set_drc(false);
                Self::Ac3(Box::new(decoder))
            }
            Codec::Opus { head } => Self::Opus(flicks_opus::Decoder::new(head).map_err(Error::decode)?),
            _ => return Err(Error::Media("unsupported audio codec")),
        })
    }

    pub fn decode(&mut self, data: &[u8], pts: f64) -> Result<Chunk> {
        let (planar, rate, layout) = match self {
            Self::Aac(d) => {
                let samples = d.decode(data).map_err(Error::decode)?;
                let layout = layout::aac(d.config().channel_config, samples.len());
                (samples, d.sample_rate(), layout)
            }
            Self::Ac3(d) => {
                let decoded = d.decode(data).map_err(Error::decode)?;
                let layout = layout::ac3(decoded.header.acmod, decoded.header.lfeon);
                (decoded.channels, decoded.header.sample_rate, layout)
            }
            Self::Opus(d) => {
                let samples = d.decode(Some(data)).map_err(Error::decode)?;
                let layout = layout::opus(d.head().mapping_family, samples.len());
                (samples, d.sample_rate(), layout)
            }
        };
        let layout = layout.ok_or(Error::Media("unsupported channel layout"))?;
        if rate == 0 {
            return Err(Error::Media("zero audio sample rate"));
        }
        let mut samples = Vec::new();
        flicks_audio::interleave(&planar, &mut samples);
        Ok(Chunk { pts, rate, layout, samples })
    }
}
