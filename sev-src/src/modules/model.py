"""Reference model: a pretrained bidirectional encoder plus a small
transformer head that reads out one logit per masked option marker.
Verified directly against Laya's `laya/common.py::DecisionModel` source:
matches `type_emb`/`head`/`scorer` construction and the marker-gather
forward pass. Deliberately omits Laya's `act_head` (a separate
action/escalation head trained with weight 0.0 and reported AUROC 0.30 —
i.e. by Laya's own account near-useless) so this checkpoint's state_dict is
not structurally loadable against `convaiinnovations/laya-typed-decisions`'s
full head, only against its shared encoder.

    ids, attention_mask (B, L)
      -> encoder (AutoModel)                -> h (B, L, D)
      -> h += type_emb(qtype)
      -> head: `head_layers` x TransformerEncoderLayer (pre-norm)
      -> gather h at marker_pos             -> (B, K, D)
      -> scorer MLP                         -> logits (B, K), pad = -1e4

`forward` returns just `logits`; option count `K` is per-batch (padded, see
`src.data.collate_fn`), not fixed like a classification head's num_classes.
"""

import torch
from torch import nn
from transformers import AutoModel

from src.config import DecisionModelConfig


class DecisionModel(nn.Module):
    def __init__(self, cfg: DecisionModelConfig, encoder: nn.Module | None = None):
        super().__init__()
        self.cfg = cfg
        self.encoder = (
            encoder
            if encoder is not None
            else AutoModel.from_pretrained(cfg.encoder_name)
        )
        d = self.encoder.config.hidden_size
        layer = nn.TransformerEncoderLayer(
            d_model=d,
            nhead=max(1, d // 64),
            dim_feedforward=4 * d,
            dropout=cfg.head_dropout,
            batch_first=True,
            norm_first=True,
        )
        # enable_nested_tensor=False matches laya/common.py::DecisionModel —
        # without it PyTorch warns and silently falls back anyway because
        # norm_first=True nested-tensor fast path isn't supported.
        self.head = nn.TransformerEncoder(
            layer, num_layers=cfg.head_layers, enable_nested_tensor=False
        )
        self.type_emb = nn.Embedding(cfg.num_qtypes, d)
        # Optional learned readout embeddings (see DecisionModelConfig). The
        # learned CLS query is prepended to the head and, after attention,
        # concatenated into the scorer alongside each option's marker state.
        self.cls_emb = (
            nn.Parameter(torch.randn(1, 1, d) * 0.02) if cfg.cls_query else None
        )
        self.slot_emb = nn.Embedding(256, d) if cfg.slot_emb else None
        scorer_in = 2 * d if cfg.cls_query else d
        self.scorer = nn.Sequential(
            nn.LayerNorm(scorer_in),
            nn.Linear(scorer_in, d),
            nn.GELU(),
            nn.Linear(d, 1),
        )
        # Per-(qtype, K-bucket) temperature, fitted post-hoc by
        # src.pipelines.eval.evaluate (via fit_temperatures +
        # fill_model_temperature) and persisted both in this buffer and as a
        # `temperatures.json` sidecar, so a checkpoint can carry its
        # calibration alongside its weights.
        num_buckets = len(cfg.k_buckets) + 1
        self.register_buffer("temperature", torch.ones(cfg.num_qtypes, num_buckets))

    def forward(
        self,
        ids: torch.Tensor,  # (B, L)
        attention_mask: torch.Tensor,  # (B, L)
        marker_pos: torch.Tensor,  # (B, K)
        marker_mask: torch.Tensor,  # (B, K)
        qtype: torch.Tensor,  # (B,)
    ) -> torch.Tensor:
        """B = batch size, L = padded token length, D = encoder hidden size,
        K = padded option count. Returns logits (B, K), masked positions
        set to -1e4.
        """
        h = self.encoder(input_ids=ids, attention_mask=attention_mask).last_hidden_state
        # h: (B, L, D)
        h = h + self.type_emb(qtype)[:, None, :]
        # type_emb(qtype): (B, D) -> [:, None, :]: (B, 1, D) broadcasts over L -> h: (B, L, D)
        batch = h.size(0)

        attn = attention_mask
        gather_pos = marker_pos
        if self.cfg.cls_query:
            # A learned query token (carrying the type signal) prepended to the
            # head; its output becomes a global context vector.
            cls = self.cls_emb.expand(batch, -1, -1) + self.type_emb(qtype)[:, None, :]
            h = torch.cat([cls, h], dim=1)
            attn = torch.cat([attention_mask.new_ones(batch, 1), attention_mask], dim=1)
            gather_pos = marker_pos + 1  # shift past the inserted query

        h = self.head(h, src_key_padding_mask=~attn.bool())
        # h: (B, L or L+1, D), self-attention over the sequence axis

        idx = gather_pos.clamp(min=0)[:, :, None].expand(-1, -1, h.size(-1))
        # marker_pos: (B, K) -> [:, :, None]: (B, K, 1) -> expand: (B, K, D)
        gathered = torch.gather(h, 1, idx)
        # gathered: (B, K, D) — one hidden vector per option marker

        if self.slot_emb is not None:
            slots = torch.arange(gathered.size(1), device=gathered.device)
            gathered = gathered + self.slot_emb(slots).unsqueeze(0)

        feats = gathered
        if self.cfg.cls_query:
            cls_out = h[:, 0, :].unsqueeze(1).expand(-1, gathered.size(1), -1)
            feats = torch.cat([gathered, cls_out], dim=-1)

        logits = self.scorer(feats).squeeze(-1).float()
        # scorer(feats): (B, K, 1) -> squeeze(-1): (B, K)
        return logits.masked_fill(~marker_mask, -1e4)
        # (B, K)
