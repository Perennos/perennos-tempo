import json
import os
from pathlib import Path
from typing import Any, Dict, Optional, Union

import torch
import yaml


def load_env(env_file: Union[str, Path] = ".env") -> None:
    """Load `KEY=VALUE` lines from `env_file` into os.environ (does not
    override already-set variables). Used by the scripts that need HF/W&B
    tokens, mirroring the reference codebase's helper.
    """
    path = Path(env_file)
    if not path.exists():
        return
    for line in path.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            key, value = line.split("=", 1)
            os.environ.setdefault(key.strip(), value.strip().strip("\"'"))


def save_checkpoint(
    model: torch.nn.Module,
    optimizer: torch.optim.Optimizer,
    step: int,
    path: Union[str, Path],
    extra: Optional[Dict[str, Any]] = None,
) -> None:
    """Save {"model", "optimizer", "step", "extra"} — the same schema
    src.pipelines.eval / infer expect to load. `extra` carries run metadata
    (config, epoch, best metric, wandb run id) that a bare state_dict loses.
    """
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    checkpoint = {
        "model": model.state_dict(),
        "optimizer": optimizer.state_dict(),
        "step": step,
        "extra": extra or {},
    }
    torch.save(checkpoint, path)


def load_checkpoint(
    path: Union[str, Path],
    model: torch.nn.Module,
    optimizer: Optional[torch.optim.Optimizer] = None,
    map_location: Optional[str] = None,
) -> dict:
    checkpoint = torch.load(path, map_location=map_location)
    model.load_state_dict(checkpoint["model"])
    if optimizer is not None and "optimizer" in checkpoint:
        optimizer.load_state_dict(checkpoint["optimizer"])
    return checkpoint


def save_json(data: Any, filename: Union[str, Path], save_pretty: bool = True) -> None:
    filename = Path(filename)
    filename.parent.mkdir(parents=True, exist_ok=True)
    with open(filename, "w") as f:
        if save_pretty:
            f.write(json.dumps(data, indent=2))
        else:
            json.dump(data, f)


def load_json(filename: Union[str, Path]) -> Any:
    with open(filename, "r") as f:
        return json.load(f)


def read_yaml(file_path: Union[str, Path]) -> Dict[str, Any]:
    file_path = Path(file_path)
    if not file_path.exists():
        raise FileNotFoundError(f"YAML file not found: {file_path}")
    with open(file_path, "r", encoding="utf-8") as f:
        data = yaml.safe_load(f)
        return data if data is not None else {}


def write_yaml(data: Dict[str, Any], file_path: Union[str, Path]) -> None:
    file_path = Path(file_path)
    file_path.parent.mkdir(parents=True, exist_ok=True)
    with open(file_path, "w", encoding="utf-8") as f:
        yaml.dump(
            data, f, default_flow_style=False, sort_keys=False, allow_unicode=True
        )
