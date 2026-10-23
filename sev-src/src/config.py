"""Model architecture for the typed-decision (RLCD/Laya-style) classifier.

This is the ARCHITECTURE-ONLY config — encoder choice, head depth, sequence
budgets. Training-loop hyperparameters (epochs, lr, loss weights, callback
wiring) live in `src.pipelines.config.TrainingConfig`, mirroring the split
used by the reference research codebase this template is modelled on.
"""

from dataclasses import dataclass

# choice: pick one of K named options. score: pick one of K ordinal levels
# (adds an RPS reward term). noul: binary yes/no probability.
QTYPES = {"choice": 0, "score": 1, "noul": 2}


@dataclass
class DecisionModelConfig:
    """A pretrained bidirectional encoder plus a small transformer head that
    reads out one logit per masked option marker.

    Layout:

        [CLS] <type> question: <instr> [SEP] [MASK] opt0 [MASK] opt1 ... [SEP] <state> [SEP]
                                                              │
                                                              ▼  encoder (bidirectional)
                                                        h += type_emb(qtype)
                                                              ▼
                                                  head: `head_layers` x TransformerEncoderLayer
                                                              ▼  gather h at [MASK] positions
                                                     scorer MLP -> 1 logit per option

    `encoder_name` is any `AutoModel`-compatible bidirectional encoder.
    Laya's real checkpoints use `answerdotai/ModernBERT-large` (English) or
    an mmBERT base (multilingual); local smoke tests use a tiny BERT
    (`google/bert_uncased_L-2_H-128_A-2`, see `configs/rlcd_smoke.yaml`) so
    the pipeline is exercisable without a GPU.
    """

    # --- encoder ---
    encoder_name: str = "answerdotai/ModernBERT-large"

    # --- head ---
    head_layers: int = 2
    head_dropout: float = 0.1
    num_qtypes: int = 3  # choice / score / noul, see QTYPES above

    # --- optional learned readout embeddings (off by default; architecture
    # experiments on top of the frozen-encoder readout) ---
    # cls_query: prepend a learned CLS query to the head and feed its output,
    #   concatenated with each option's marker state, into the scorer.
    # slot_emb: add a learned per-option-slot embedding to each marker state.
    cls_query: bool = False
    slot_emb: bool = False

    # --- sequence budgets ---
    max_len: int = 512
    head_max_len: int = 192  # 192 EN / 256 multilingual in Laya's real checkpoints

    # option-count bucket boundaries for per-(type, K-bucket) temperature:
    # buckets are 2 / 3..k_buckets[1] / (k_buckets[1]+1)..k_buckets[2] / 11+
    k_buckets: tuple[int, ...] = (2, 5, 10)

    def __post_init__(self) -> None:
        if self.head_layers < 1:
            raise ValueError(f"head_layers must be >= 1, got {self.head_layers}")
        if not 0.0 <= self.head_dropout < 1.0:
            raise ValueError(f"head_dropout must be in [0, 1), got {self.head_dropout}")
        if self.num_qtypes != len(QTYPES):
            raise ValueError(f"num_qtypes must be {len(QTYPES)}, got {self.num_qtypes}")
        if self.max_len <= 0 or self.head_max_len <= 0:
            raise ValueError("max_len and head_max_len must be > 0")
        if self.head_max_len > self.max_len:
            raise ValueError("head_max_len must be <= max_len")
