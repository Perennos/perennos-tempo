import random
import string
from datetime import datetime

import torch


def get_run_name(
    model_name: str,
    dset_name: str,
    lr: float,
    batch_size: int,
    experiment_type: str = "",
    random_suffix: bool = True,
    random_suffix_len: int = 6,
) -> str:
    """Generate a unique, sortable run name with a timestamp (YYYY-MM-DD)."""
    now = datetime.now().strftime("%Y-%m-%d")
    rand_suffix = (
        "".join(
            random.Random().choices(
                string.ascii_letters + string.digits, k=random_suffix_len
            )
        )
        if random_suffix
        else ""
    )
    return f"{model_name}_{dset_name}_{experiment_type}_lr{lr}_bs{batch_size}_{now}_{rand_suffix}"


def count_parameters(model: torch.nn.Module, verbose: bool = True) -> dict:
    """Count parameters in a PyTorch model.

    from src.utils.model_utils import count_parameters
    count_parameters(model)
    """
    n_all = sum(p.numel() for p in model.parameters())
    n_trainable = sum(p.numel() for p in model.parameters() if p.requires_grad)
    n_frozen = n_all - n_trainable
    if verbose:
        print(
            "Parameter Count: all {:,d}; trainable {:,d}; frozen {:,d}".format(
                n_all, n_trainable, n_frozen
            )
        )
    return {
        "n_all": n_all,
        "n_trainable": n_trainable,
        "n_frozen": n_frozen,
    }


def detect_device() -> torch.device:
    if torch.cuda.is_available():
        return torch.device("cuda")
    elif torch.backends.mps.is_available():
        return torch.device("mps")
    else:
        return torch.device("cpu")
