"""Training loss and the logit-derived helpers used at evaluation time.

`rlcd_loss` is a faithful port of Laya's training step (verified against
`laya/common.py` and its training loop): a score-function
(evolution-strategies) estimate of the gradient of a noise-smoothed proper
score, plus a soft-target cross-entropy term. `w_rl=0` gives CE-only;
`w_ce=0` gives RL-only — the loss-ablation configs this project studies are
exactly these two weights (and `sigma`).
"""

import torch
import torch.nn.functional as F

from src.config import QTYPES


def proper_reward(
    q: torch.Tensor,
    t: torch.Tensor,
    qtype: torch.Tensor,
    mask: torch.Tensor,
    w_sph: float = 0.5,
    w_rps: float = 1.0,
) -> torch.Tensor:
    """log score + w_sph * spherical score - w_rps * RPS (RPS only for
    score-type questions). `w_sph=0.5` matches the default in
    `laya/common.py::proper_reward`; note Laya's typed-decisions fine-tune
    notebook (`laya_finetune_typed_decisions_2xT4_kaggle.ipynb`) overrides it
    with `w_sph=0.75`, which is what the E2 configs use.

    ... denotes arbitrary leading dims (e.g. a noise-sample axis `G`),
    broadcast the same way across q/t/qtype/mask.
    """
    mask_f = mask.to(q.dtype)  # mask: (..., K) bool -> (..., K) float
    q = q * mask_f  # q: (..., K)
    log_s = (t * torch.log(q.clamp_min(1e-12)).clamp_min(-9.21)).sum(-1)
    # log_s: (...,) — summed out the K axis
    sph = (t * q).sum(-1) / q.norm(dim=-1).clamp_min(1e-9)
    # sph: (...,)
    k = mask_f.sum(-1).clamp(min=2)
    # k: (...,) — valid-option count per row
    rps = (((q.cumsum(-1) - t.cumsum(-1)) ** 2) * mask_f).sum(-1) / (k - 1)
    # rps: (...,)
    is_score = (qtype == QTYPES["score"]).to(q.dtype)
    # is_score: (...,) or broadcastable to it (qtype is usually (B,), r is (G, B))
    return log_s + w_sph * sph - w_rps * rps * is_score
    # (...,)


def rlcd_loss(
    z: torch.Tensor,  # (B, K)
    target: torch.Tensor,  # (B, K)
    qtype: torch.Tensor,  # (B,)
    mask: torch.Tensor,  # (B, K)
    sigma: float,
    num_noise_samples: int = 4,
    w_ce: float = 1.0,
    w_rl: float = 1.0,
    w_sph: float = 0.5,
    w_rps: float = 1.0,
) -> tuple[torch.Tensor, dict[str, float]]:
    """B = batch size, K = padded option count, G = `num_noise_samples`.

    RL term: G noisy copies of z, projected to zero-mean over valid
    options; reward each; standardize into a group-baselined advantage;
    score-function gradient via `-adv * log N(z+eps | z, sigma^2)`.
    CE term: soft cross-entropy of z against the target distribution.
    """
    mask_bool = mask.bool()  # (B, K)
    mask_f = mask_bool.to(z.dtype)  # (B, K)
    k = mask_f.sum(-1, keepdim=True).clamp(min=2)
    # k: (B, 1) — valid-option count per row

    eps = torch.randn((num_noise_samples,) + z.shape, device=z.device, dtype=z.dtype)
    # eps: (G, B, K)
    eps = eps * sigma * mask_f
    # (G, B, K), mask_f (B, K) broadcasts over the new G axis
    eps = (eps - eps.sum(-1, keepdim=True) / k) * mask_f
    # eps.sum(-1, keepdim=True): (G, B, 1); result stays (G, B, K), zero-mean per row
    zs = z.detach().unsqueeze(0) + eps
    # z.detach().unsqueeze(0): (1, B, K) + eps (G, B, K) -> zs: (G, B, K)
    q = torch.softmax(zs.masked_fill(~mask_bool, -1e4), dim=-1)
    # q: (G, B, K)

    with torch.no_grad():
        r = proper_reward(q, target.unsqueeze(0), qtype, mask_bool, w_sph, w_rps)
        # target.unsqueeze(0): (1, B, K) broadcasts against q (G, B, K) -> r: (G, B)
        adv = r - r.mean(0, keepdim=True)
        # r.mean(0, keepdim=True): (1, B); adv: (G, B) — group-baselined advantage
        adv = adv / (adv.std() + 1e-6)
        # Normalised by the std of the *centred* advantages, as Laya's typed-
        # decisions fine-tune notebook does — not by r.std(), which also counts
        # between-row reward spread and so shrinks the RL term.

    logp = -(((zs - z.unsqueeze(0)) ** 2) * mask_f).sum(-1) / (2 * sigma**2)
    # zs (G, B, K) - z.unsqueeze(0) (1, B, K) -> (G, B, K) -> sum(-1) -> logp: (G, B)
    loss_rl = -(adv * logp).mean()
    # scalar

    z_masked = z.masked_fill(~mask_bool, -1e4)  # (B, K)
    loss_ce = -(target * torch.log_softmax(z_masked, dim=-1)).sum(-1).mean()
    # (B, K) -> sum(-1): (B,) -> mean(): scalar

    total = w_rl * loss_rl + w_ce * loss_ce  # scalar
    return total, {
        "loss_rl": float(loss_rl.detach()),
        "loss_ce": float(loss_ce.detach()),
    }


@torch.no_grad()
def probabilities(logits: torch.Tensor) -> torch.Tensor:
    """Softmax probabilities over the padded option dimension, (B, K).
    Padded positions carry logit -1e4 (see `DecisionModel.forward`) and so
    receive ~0 probability.
    """
    return F.softmax(logits, dim=-1)


@torch.no_grad()
def predict(logits: torch.Tensor) -> torch.Tensor:
    """Argmax option index per row, (B,)."""
    return logits.argmax(dim=-1)
