"""Dataset plumbing for typed probabilistic decisions.

One raw case is a JSON `state` blob plus several typed *questions* (choice /
score / noul), each with a gold soft-target distribution over its options.
`TypedDecisionDataset` flattens this into one row per (case, question) pair
— matching Laya's own sequence-builder, which re-encodes the state once per
question rather than once per case (so latency is linear in question count).

    state + {q1: choice, q2: score, q3: noul}
            -> 3 rows, each `[CLS] <type> question: ... [SEP] [MASK] opt0 ... [SEP] <state> [SEP]`

Replace `TypedDecisionDataset` / `build_sequence` when bringing a different
typed-decision source; the rest of the pipeline only needs a `Dataset`
yielding the dict shape documented on `TypedDecisionDataset.__getitem__`
plus a matching `collate_fn`.
"""

import json

import torch
from torch.utils.data import Dataset

from src.config import QTYPES

# `LocalLLaMA/typed-decisions` sometimes omits `criteria` for `noul`
# questions (a plain yes/no with no dataset-provided wording) — verified by
# inspecting the real dataset, not assumed. Falls back to Laya's own
# default false/true wording (laya/common.py::render_options).
_DEFAULT_NOUL_CRITERIA = {
    "false": "no, the statement does not hold",
    "true": "yes, the statement holds",
}


def _noul_criteria(question: dict) -> dict:
    return question.get("criteria") or _DEFAULT_NOUL_CRITERIA


def render_options(question: dict) -> list[str]:
    """Render a question's options as strings, in the same order used for
    both the sequence's option markers and the target vector (`target_vector`).

    `choice` criteria are `{option_key: description}` dicts (order preserved
    from the source JSON). `noul` criteria are the same shape when present,
    else the fixed false/true wording above. `score` criteria are an
    ordinal list of level descriptions.
    """
    if question["type"] == "choice":
        return [f"{k}: {v}" for k, v in question["criteria"].items()]
    if question["type"] == "noul":
        return [f"{k}: {v}" for k, v in _noul_criteria(question).items()]
    if question["type"] == "score":
        return [f"level {i}: {c}" for i, c in enumerate(question["criteria"])]
    raise ValueError(f"unknown question type: {question['type']!r}")


def option_keys(question: dict) -> list[str]:
    """The keys used to index `gold[qid]["probabilities"]`, in the same
    order as `render_options` — needed to build a target vector aligned to
    the option markers.
    """
    if question["type"] == "choice":
        return list(question["criteria"].keys())
    if question["type"] == "noul":
        return list(_noul_criteria(question).keys())
    if question["type"] == "score":
        return [str(i) for i in range(len(question["criteria"]))]
    raise ValueError(f"unknown question type: {question['type']!r}")


def target_vector(question: dict, gold_entry: dict) -> list[float]:
    probs = gold_entry["probabilities"]
    return [float(probs[k]) for k in option_keys(question)]


def _permute_question(question: dict) -> dict:
    """Shallow-copy `question` with choice/noul option order shuffled.

    `target_vector`/`label_index` index by option *key*, so they follow the
    shuffled order automatically — the augmentation is label-preserving.
    """
    items = list(question["criteria"].items())
    order = torch.randperm(len(items)).tolist()
    q = dict(question)
    q["criteria"] = {items[i][0]: items[i][1] for i in order}
    return q


def label_index(question: dict, gold_entry: dict) -> int:
    """Index of the gold `label` among `option_keys`, or -1 if absent.

    Laya's evaluation scores accuracy/ECE against this hard label, not the
    soft target's argmax (they disagree on ~1% of `LocalLLaMA/typed-decisions`
    rows), so eval uses it when present.
    """
    label = gold_entry.get("label")
    if label is None:
        return -1
    keys = [k.lower() for k in option_keys(question)]
    label = str(label).lower()
    return keys.index(label) if label in keys else -1


def build_sequence(
    tokenizer, state: str, question: dict, max_len: int = 512, head_max_len: int = 192
) -> tuple[list[int], list[int]]:
    """Build one encoder input row for `question` against `state`.

        [CLS] <type> question: <instructions> [SEP] [MASK] opt0 [MASK] opt1 ... [SEP] <state> [SEP]

    Returns `(token_ids, marker_positions)` where `marker_positions[i]` is
    the index of option `i`'s `[MASK]` token in `token_ids` — the position
    `src.modules.model.DecisionModel` reads a logit out of.
    """
    mask_str = tokenizer.mask_token
    # If the instructions/options/state text happens to literally contain
    # the tokenizer's mask string, blank it out first — otherwise it tokenizes
    # into a real mask token and desyncs `marker_positions` from the actual
    # option markers (matches laya/common.py::build_sequence's same guard).
    instructions = str(question["instructions"]).replace(mask_str, " ")
    head_ids = tokenizer(
        f"{question['type']} question: {instructions}", add_special_tokens=False
    )["input_ids"]

    opts = []
    for option in render_options(question):
        opt_ids = tokenizer(
            " " + option.replace(mask_str, " "),
            add_special_tokens=False,
            truncation=True,
            max_length=48,
        )["input_ids"]
        opts.append([tokenizer.mask_token_id] + opt_ids)

    budget = head_max_len - sum(len(o) for o in opts)
    if budget < 16:
        # Too many/long options for the shared option budget: truncate each
        # option to a fixed share (this is the branch a large option set,
        # e.g. Banking77's 77 classes, triggers hard).
        per = max(4, (head_max_len - 16) // len(opts))
        opts = [o[:per] for o in opts]
        budget = head_max_len - sum(len(o) for o in opts)

    ids = (
        [tokenizer.cls_token_id] + head_ids[: max(8, budget)] + [tokenizer.sep_token_id]
    )
    markers = []
    for opt_ids in opts:
        markers.append(len(ids))
        ids += opt_ids
    ids.append(tokenizer.sep_token_id)

    state_ids = tokenizer(str(state).replace(mask_str, " "), add_special_tokens=False)[
        "input_ids"
    ]
    state_ids = state_ids[: max(0, max_len - len(ids) - 1)]
    ids = (ids + state_ids + [tokenizer.sep_token_id])[:max_len]
    # Safety net: state truncation above already keeps markers in range in
    # practice, but drop any that somehow land past the final cutoff rather
    # than let DecisionModel gather from a truncated-away position.
    markers = [m for m in markers if m < len(ids)]
    return ids, markers


class TypedDecisionDataset(Dataset):
    """Flattens an HF `LocalLLaMA/typed-decisions`-shaped split (columns
    `state`, `questions`, `gold`, each a JSON string) into one row per
    (case, question).

    `__getitem__` returns `{"ids", "markers", "target", "qtype", "label"}` —
    variable length per row; use `collate_fn` to batch.
    """

    def __init__(
        self,
        hf_dataset,
        tokenizer,
        max_len: int = 512,
        head_max_len: int = 192,
        augment_permute: bool = False,
    ):
        self.tokenizer = tokenizer
        self.max_len = max_len
        self.head_max_len = head_max_len
        self.augment_permute = augment_permute
        self._rows: list[tuple[str, dict, dict]] = []
        for example in hf_dataset:
            state = example["state"]
            questions = json.loads(example["questions"])
            gold = json.loads(example["gold"])
            for qid, question in questions.items():
                self._rows.append((state, question, gold[qid]))

    def __len__(self) -> int:
        return len(self._rows)

    def __getitem__(self, index: int) -> dict[str, object]:
        state, question, gold_entry = self._rows[index]
        if self.augment_permute and question["type"] in ("choice", "noul"):
            if "criteria" in question:
                question = _permute_question(question)
        ids, markers = build_sequence(
            self.tokenizer, state, question, self.max_len, self.head_max_len
        )
        return {
            "ids": ids,
            "markers": markers,
            "target": target_vector(question, gold_entry),
            "qtype": QTYPES[question["type"]],
            "label": label_index(question, gold_entry),
        }


def collate_fn(
    batch: list[dict[str, object]], pad_token_id: int
) -> dict[str, torch.Tensor]:
    """Pad a list of `TypedDecisionDataset` rows to the batch's max sequence
    length / option count. Returns tensors keyed `ids`, `attention_mask`,
    `marker_pos`, `marker_mask`, `target`, `qtype` — the exact kwargs
    `src.modules.model.DecisionModel.forward` and `src.modules.loss.rlcd_loss`
    expect — plus `label` (gold option index, -1 when a row has none).
    """
    batch_size = len(batch)
    max_len = max(len(row["ids"]) for row in batch)
    max_opts = max(len(row["markers"]) for row in batch)

    ids = torch.full((batch_size, max_len), pad_token_id, dtype=torch.long)
    attention_mask = torch.zeros((batch_size, max_len), dtype=torch.long)
    marker_pos = torch.zeros((batch_size, max_opts), dtype=torch.long)
    marker_mask = torch.zeros((batch_size, max_opts), dtype=torch.bool)
    target = torch.zeros((batch_size, max_opts), dtype=torch.float32)
    qtype = torch.zeros((batch_size,), dtype=torch.long)
    label = torch.full((batch_size,), -1, dtype=torch.long)

    for i, row in enumerate(batch):
        seq_len = len(row["ids"])
        num_opts = len(row["markers"])
        ids[i, :seq_len] = torch.as_tensor(row["ids"], dtype=torch.long)
        attention_mask[i, :seq_len] = 1
        marker_pos[i, :num_opts] = torch.as_tensor(row["markers"], dtype=torch.long)
        marker_mask[i, :num_opts] = True
        target[i, :num_opts] = torch.as_tensor(row["target"], dtype=torch.float32)
        qtype[i] = row["qtype"]
        label[i] = row.get("label", -1)

    return {
        "ids": ids,
        "attention_mask": attention_mask,
        "marker_pos": marker_pos,
        "marker_mask": marker_mask,
        "target": target,
        "qtype": qtype,
        "label": label,
    }


def split_train_calib(hf_train_dataset, calib_fraction: float, seed: int):
    """Deterministic case-level split of the HF train split into (train,
    calibration) — the calibration slice is what `src.pipelines.eval`
    fits the per-(type, K-bucket) temperature on, never the test split
    (Laya's own issue #186 warns fitting temperature on training items
    inflates it). Returns `(train, None)` when `calib_fraction <= 0`.
    """
    if calib_fraction <= 0:
        return hf_train_dataset, None
    split = hf_train_dataset.train_test_split(test_size=calib_fraction, seed=seed)
    return split["train"], split["test"]
