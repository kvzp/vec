//! Embedding canary and runtime history.
//!
//! The model identity ([`crate::ModelMismatch`]) catches another model. It
//! cannot catch the same model computed differently: an Ollama upgrade, a
//! tract upgrade, or (when the server reports no digest) a model re-pulled
//! with other weights. Those can move every vector a little, so new vectors
//! no longer compare exactly with the stored ones.
//!
//! The canary measures that. The first write to an index stores the vector
//! of a fixed text (`[embed] canary_text`) with the runtime that computed it
//! (`meta` keys `canary.text`, `canary.vector`, `canary.runtime`). A check
//! embeds the stored text again and compares (cosine) with the stored vector:
//! at least `[embed] canary_warn_cosine` is [`CanaryVerdict::Ok`], at least
//! `canary_min_cosine` is [`CanaryVerdict::Warn`], below is
//! [`CanaryVerdict::Drift`], which writers turn into an [`EmbeddingDrift`]
//! error.
//!
//! The runtime history (`runtime.first_write`, `runtime.last_write`) records
//! which runtime wrote the index first (since its creation, its last model
//! reset or its last full rebuild) and last. It is informational: decisions
//! use the canary only.

use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

use crate::{Store, dot, get_meta, normalize, set_meta, write_tx};

/// `meta` key of the canary text.
const META_CANARY_TEXT: &str = "canary.text";
/// `meta` key of the canary vector (space-separated f32, normalised).
const META_CANARY_VECTOR: &str = "canary.vector";
/// `meta` key of the runtime that computed the canary vector.
const META_CANARY_RUNTIME: &str = "canary.runtime";
/// `meta` key of the runtime of the first write.
const META_RUNTIME_FIRST: &str = "runtime.first_write";
/// `meta` key of the runtime of the last write.
const META_RUNTIME_LAST: &str = "runtime.last_write";

/// Every key of this module: a model reset removes them all.
pub(crate) const META_KEYS: &[&str] = &[
    META_CANARY_TEXT,
    META_CANARY_VECTOR,
    META_CANARY_RUNTIME,
    META_RUNTIME_FIRST,
    META_RUNTIME_LAST,
];

/// Thresholds of a canary check (`[embed] canary_warn_cosine`,
/// `canary_min_cosine`; validated by the config: 0 < min <= warn <= 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanaryLimits {
    /// At or above: ok. Below: warning.
    pub warn_cosine: f32,
    /// Below: drift.
    pub min_cosine: f32,
}

impl CanaryLimits {
    /// The limits of the `[embed]` config section.
    #[must_use]
    pub fn from_config(e: &vec_core::config::EmbedConfig) -> Self {
        Self {
            warn_cosine: e.canary_warn_cosine,
            min_cosine: e.canary_min_cosine,
        }
    }
}

/// A stored canary: the text, its vector, and the runtime that computed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Canary {
    /// The text that was embedded.
    pub text: String,
    /// Its vector (stored normalised).
    pub vector: Vec<f32>,
    /// [`vec_embed::Embedder::runtime`] when it was computed.
    pub runtime: Option<String>,
}

/// How far the vectors moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryVerdict {
    /// Cosine at or above the warning limit.
    Ok,
    /// Between the two limits: worth a warning, still usable.
    Warn,
    /// Below the minimum: new vectors must not be mixed with the index's.
    Drift,
}

/// The result of re-embedding the canary.
#[derive(Debug, Clone, PartialEq)]
pub struct CanaryCheck {
    /// Cosine between the stored and the new vector.
    pub cosine: f32,
    /// What the cosine means under [`CanaryCheck::limits`].
    pub verdict: CanaryVerdict,
    /// The limits applied.
    pub limits: CanaryLimits,
    /// The runtime that computed the stored vector.
    pub recorded_runtime: Option<String>,
    /// The runtime that computed the new vector.
    pub current_runtime: Option<String>,
}

impl CanaryCheck {
    /// Compare `fresh` (the stored text embedded now by `current_runtime`)
    /// with `stored`. A non-finite cosine counts as drift.
    ///
    /// # Errors
    ///
    /// The vectors have different dimensions (another model: a model
    /// identity check should have refused it first).
    pub fn compare(
        stored: &Canary,
        fresh: &[f32],
        current_runtime: Option<String>,
        limits: CanaryLimits,
    ) -> Result<Self> {
        let cosine = dot(&normalize(&stored.vector), &normalize(fresh))
            .context("comparing the embedding canary (another model?)")?;
        let verdict = if cosine >= limits.warn_cosine {
            CanaryVerdict::Ok
        } else if cosine >= limits.min_cosine {
            CanaryVerdict::Warn
        } else {
            CanaryVerdict::Drift
        };
        Ok(Self {
            cosine,
            verdict,
            limits,
            recorded_runtime: stored.runtime.clone(),
            current_runtime,
        })
    }

    /// "recorded with X, now Y" (`unknown` for a runtime that is not known).
    #[must_use]
    pub fn runtimes(&self) -> String {
        let name = |r: &Option<String>| r.clone().unwrap_or_else(|| "unknown".to_owned());
        format!(
            "runtime when recorded: {}, now: {}",
            name(&self.recorded_runtime),
            name(&self.current_runtime)
        )
    }
}

impl fmt::Display for CanaryCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "embedding canary cosine {:.6} (ok from {}, refused below {}); {}",
            self.cosine,
            self.limits.warn_cosine,
            self.limits.min_cosine,
            self.runtimes()
        )
    }
}

/// The embedding backend no longer computes the vectors the index holds:
/// its canary moved below `[embed] canary_min_cosine`. Returned by writers
/// inside their `anyhow::Error`; find it with
/// `err.downcast_ref::<EmbeddingDrift>()`.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingDrift {
    /// The database file.
    pub db_path: PathBuf,
    /// The failed check.
    pub check: CanaryCheck,
}

impl fmt::Display for EmbeddingDrift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "embedding drift: the model now embeds the canary text of the index at {} with \
             cosine {:.6} to the stored vector (minimum {}); {}. The model was re-pulled with \
             other weights or its runtime changed, so new vectors would not compare with the \
             stored ones. Nothing was written. Re-embed everything with `vec updatedb --full`, \
             or restore the previous runtime.",
            self.db_path.display(),
            self.check.cosine,
            self.check.limits.min_cosine,
            self.check.runtimes()
        )
    }
}

impl std::error::Error for EmbeddingDrift {}

/// Which runtime wrote the index (`None`: not recorded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeHistory {
    /// The first write since the index was created, reset for another model,
    /// or fully rebuilt. For an index older than this record: the first write
    /// after the upgrade.
    pub first_write: Option<String>,
    /// The most recent write.
    pub last_write: Option<String>,
}

impl Store {
    /// The stored canary, if any.
    ///
    /// # Errors
    ///
    /// A database error, or a corrupt stored vector.
    pub fn canary(&self) -> Result<Option<Canary>> {
        let (Some(text), Some(vector)) = (
            get_meta(&self.conn, META_CANARY_TEXT)?,
            get_meta(&self.conn, META_CANARY_VECTOR)?,
        ) else {
            return Ok(None);
        };
        let vector = vector
            .split_ascii_whitespace()
            .map(str::parse::<f32>)
            .collect::<Result<Vec<f32>, _>>()
            .map_err(|e| anyhow!("meta.{META_CANARY_VECTOR} is corrupt: {e}"))?;
        Ok(Some(Canary {
            text,
            vector,
            runtime: get_meta(&self.conn, META_CANARY_RUNTIME)?.filter(|r| !r.is_empty()),
        }))
    }

    /// Store `canary` (replacing any previous one), in one transaction.
    ///
    /// # Errors
    ///
    /// An empty text, an empty or non-finite vector, or a database error.
    pub fn set_canary(&mut self, canary: &Canary) -> Result<()> {
        if canary.text.is_empty() {
            bail!("the canary text is empty");
        }
        if canary.vector.is_empty() || canary.vector.iter().any(|x| !x.is_finite()) {
            bail!("the canary vector is empty or not finite");
        }
        let vector: Vec<String> = normalize(&canary.vector)
            .iter()
            .map(f32::to_string)
            .collect();
        let tx = write_tx(&mut self.conn)?;
        set_meta(&tx, META_CANARY_TEXT, &canary.text)?;
        set_meta(&tx, META_CANARY_VECTOR, &vector.join(" "))?;
        set_meta(
            &tx,
            META_CANARY_RUNTIME,
            canary.runtime.as_deref().unwrap_or_default(),
        )?;
        tx.commit().context("committing the embedding canary")
    }

    /// Re-embed the stored canary text with `embed` and compare. `None` when
    /// the index has no canary. Never writes: callers decide what a
    /// [`CanaryVerdict::Drift`] means (writers refuse, readers report).
    ///
    /// # Errors
    ///
    /// A database error, `embed` failing, or a vector of another dimension.
    pub fn check_canary(
        &self,
        embed: impl FnOnce(&str) -> Result<Vec<f32>>,
        current_runtime: Option<String>,
        limits: CanaryLimits,
    ) -> Result<Option<CanaryCheck>> {
        let Some(stored) = self.canary()? else {
            return Ok(None);
        };
        let fresh = embed(&stored.text).context("embedding the canary text")?;
        CanaryCheck::compare(&stored, &fresh, current_runtime, limits).map(Some)
    }

    /// [`EmbeddingDrift`] for `check` on this index (for writers).
    #[must_use]
    pub fn drift_error(&self, check: CanaryCheck) -> EmbeddingDrift {
        EmbeddingDrift {
            db_path: self.path.clone(),
            check,
        }
    }

    /// The recorded runtimes.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn runtime_history(&self) -> Result<RuntimeHistory> {
        let get = |k| Ok::<_, anyhow::Error>(get_meta(&self.conn, k)?.filter(|v| !v.is_empty()));
        Ok(RuntimeHistory {
            first_write: get(META_RUNTIME_FIRST)?,
            last_write: get(META_RUNTIME_LAST)?,
        })
    }

    /// Record `runtime` as the runtime of the last write, and of the first
    /// write when none is recorded or `whole_index` (a full rebuild just
    /// rewrote every vector).
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn record_write_runtime(&mut self, runtime: &str, whole_index: bool) -> Result<()> {
        let tx = write_tx(&mut self.conn)?;
        set_meta(&tx, META_RUNTIME_LAST, runtime)?;
        if whole_index || get_meta(&tx, META_RUNTIME_FIRST)?.is_none() {
            set_meta(&tx, META_RUNTIME_FIRST, runtime)?;
        }
        tx.commit().context("committing the runtime record")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StoreOptions;
    use std::path::Path;

    /// Test limits (the defaults.toml values are not needed here).
    const LIMITS: CanaryLimits = CanaryLimits {
        warn_cosine: 0.999,
        min_cosine: 0.98,
    };

    fn store() -> Store {
        let opts = StoreOptions {
            wal: false,
            busy_timeout_ms: 1_000,
            read_only: false,
            migration_wait_ms: 1_000,
        };
        Store::open(Path::new(":memory:"), &opts).unwrap()
    }

    fn canary(vector: Vec<f32>) -> Canary {
        Canary {
            text: "canary".into(),
            vector,
            runtime: Some("rt 1".into()),
        }
    }

    /// A unit vector at angle `theta` from [1, 0, 0].
    fn at_angle(theta: f32) -> Vec<f32> {
        vec![theta.cos(), theta.sin(), 0.0]
    }

    #[test]
    fn canary_roundtrips_exactly_and_is_normalised() {
        let mut s = store();
        assert_eq!(s.canary().unwrap(), None);
        let v = vec![0.1f32, -0.3, 0.7];
        s.set_canary(&canary(v.clone())).unwrap();
        let got = s.canary().unwrap().unwrap();
        assert_eq!(got.vector, normalize(&v), "f32 text form is lossless");
        assert_eq!(got.runtime.as_deref(), Some("rt 1"));
        assert!(s.set_canary(&canary(vec![f32::NAN])).is_err());
        assert!(s.set_canary(&canary(Vec::new())).is_err());
    }

    #[test]
    fn verdicts_follow_the_limits() {
        let mut s = store();
        s.set_canary(&canary(at_angle(0.0))).unwrap();
        let check = |theta: f32| {
            s.check_canary(|_| Ok(at_angle(theta)), Some("rt 2".into()), LIMITS)
                .unwrap()
                .unwrap()
        };
        assert_eq!(check(0.0).verdict, CanaryVerdict::Ok);
        // cos 0.03 = 0.99955: ok; cos 0.1 = 0.995: warn; cos 0.3 = 0.955: drift.
        assert_eq!(check(0.03).verdict, CanaryVerdict::Ok);
        let warn = check(0.1);
        assert_eq!(warn.verdict, CanaryVerdict::Warn);
        assert!(warn.to_string().contains("now: rt 2"), "{warn}");
        let drift = check(0.3);
        assert_eq!(drift.verdict, CanaryVerdict::Drift);
        let err = anyhow::Error::from(s.drift_error(drift));
        assert!(err.is::<EmbeddingDrift>());
        let msg = err.to_string();
        assert!(
            msg.contains("vec updatedb --full") && msg.contains("rt 1"),
            "{msg}"
        );
    }

    #[test]
    fn check_embeds_the_stored_text_and_rejects_another_dimension() {
        let mut s = store();
        assert_eq!(
            s.check_canary(|_| unreachable!(), None, LIMITS).unwrap(),
            None
        );
        s.set_canary(&canary(at_angle(0.0))).unwrap();
        let mut seen = String::new();
        s.check_canary(
            |t| {
                seen = t.to_owned();
                Ok(at_angle(0.0))
            },
            None,
            LIMITS,
        )
        .unwrap();
        assert_eq!(seen, "canary");
        assert!(s.check_canary(|_| Ok(vec![1.0; 4]), None, LIMITS).is_err());
        let nan = s.check_canary(|_| Ok(vec![f32::NAN; 3]), None, LIMITS);
        assert_eq!(
            nan.unwrap().unwrap().verdict,
            CanaryVerdict::Drift,
            "NaN is drift"
        );
    }

    #[test]
    fn runtime_history_keeps_the_first_until_a_whole_rewrite() {
        let mut s = store();
        assert_eq!(s.runtime_history().unwrap(), RuntimeHistory::default());
        s.record_write_runtime("a", false).unwrap();
        s.record_write_runtime("b", false).unwrap();
        let h = s.runtime_history().unwrap();
        assert_eq!(
            (h.first_write.as_deref(), h.last_write.as_deref()),
            (Some("a"), Some("b"))
        );
        s.record_write_runtime("c", true).unwrap();
        let h = s.runtime_history().unwrap();
        assert_eq!(
            (h.first_write.as_deref(), h.last_write.as_deref()),
            (Some("c"), Some("c"))
        );
    }
}
