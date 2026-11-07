//! The training script, as text the owner can read before anything runs. Tempo plans, runs,
//! checks and promotes; the training itself is Python, and this file writes it.
//!
//! On an NVIDIA card: QLoRA with TRL (4-bit base, rank 8 to 16, one or two passes, the attention
//! and MLP projections only). On a Mac: `mlx_lm.lora` with the same projections. Either way the
//! adapter is turned into a GGUF with llama.cpp's `convert_lora_to_gguf.py`, which is what Ollama's
//! `ADAPTER` line and llama-server's `--lora` both take.
//!
//! None of this was run for real here (no GPU, and the downloads are blocked on the machine it was
//! written on). The tests check the text and the commands; the trainer itself is a fake there.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Cuda,
    Mlx,
}

impl Backend {
    pub fn words(self) -> &'static str {
        match self {
            Backend::Cuda => "an NVIDIA card (QLoRA with TRL)",
            Backend::Mlx => "Apple Silicon (mlx_lm.lora)",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub band: String,
    pub backend: Backend,
    /// The Ollama model the adapter will sit on.
    pub base: String,
    /// The same model's weights on Hugging Face, which is what the trainer reads.
    pub hf: String,
    pub rank: u32,
    pub epochs: u32,
    pub train_examples: usize,
}

/// The projections that get an adapter: attention and MLP, nothing else.
pub const TARGETS: [&str; 7] = [
    "q_proj",
    "k_proj",
    "v_proj",
    "o_proj",
    "gate_proj",
    "up_proj",
    "down_proj",
];

/// The Hugging Face weights for an Ollama tag of a family Tempo knows. Anything else: the owner
/// says (`tempo-launch woodshed set <band> hf <repo>`). Not checked against the Hub here.
pub fn hf_for(ollama: &str) -> Option<String> {
    let (name, tag) = ollama.split_once(':')?;
    let size = tag.split(['-', '_']).next()?.to_ascii_lowercase();
    let b = size.strip_suffix('b')?;
    b.parse::<f64>().ok()?;
    let up = format!("{b}B");
    match name {
        "qwen3" => Some(format!("Qwen/Qwen3-{up}")),
        "llama3.2" => Some(format!("meta-llama/Llama-3.2-{up}-Instruct")),
        "gemma3" => Some(format!("google/gemma-3-{b}b-it")),
        _ => None,
    }
}

/// The 4-bit MLX copy of a Hugging Face repo, by the mlx-community naming.
pub fn mlx_repo(hf: &str) -> String {
    format!("mlx-community/{}-4bit", hf.rsplit('/').next().unwrap_or(hf))
}

pub fn venv_bin(venv: &str, name: &str, windows: bool) -> String {
    if windows {
        format!("{venv}\\Scripts\\{name}.exe")
    } else {
        format!("{venv}/bin/{name}")
    }
}

/// How many steps `mlx_lm.lora` runs for `epochs` passes over `n` examples (batch size 1).
pub fn mlx_iters(n: usize, epochs: u32) -> usize {
    (n * epochs as usize).max(1)
}

/// The files written beside the data: (name, text).
pub fn files(p: &Plan) -> Vec<(String, String)> {
    match p.backend {
        Backend::Cuda => vec![("train.py".into(), train_py(p))],
        Backend::Mlx => vec![
            ("lora.yaml".into(), lora_yaml(p)),
            ("mlx_to_peft.py".into(), MLX_TO_PEFT.into()),
        ],
    }
}

fn train_py(p: &Plan) -> String {
    let targets = TARGETS
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"# Tempo Woodshed: QLoRA for the "{band}" band. Written by tempo-launch; read it, change it, run it yourself if you like.
# The base is {base} (Ollama), trained from its weights on Hugging Face: {hf}.
import torch
from datasets import load_dataset
from peft import LoraConfig
from transformers import AutoModelForCausalLM, AutoTokenizer, BitsAndBytesConfig
from trl import SFTConfig, SFTTrainer

BASE = "{hf}"
tok = AutoTokenizer.from_pretrained(BASE)
model = AutoModelForCausalLM.from_pretrained(
    BASE,
    quantization_config=BitsAndBytesConfig(
        load_in_4bit=True,
        bnb_4bit_quant_type="nf4",
        bnb_4bit_compute_dtype=torch.bfloat16,
        bnb_4bit_use_double_quant=True,
    ),
    device_map={{"": 0}},
)
train = load_dataset("json", data_files="train.jsonl", split="train")
valid = load_dataset("json", data_files="valid.jsonl", split="train")

# attention and MLP projections only, rank {rank}
lora = LoraConfig(
    r={rank},
    lora_alpha={alpha},
    lora_dropout=0.05,
    target_modules=[{targets}],
    task_type="CAUSAL_LM",
)
args = SFTConfig(
    output_dir="out",
    num_train_epochs={epochs},
    per_device_train_batch_size=1,
    gradient_accumulation_steps=8,
    learning_rate=1e-4,
    lr_scheduler_type="cosine",
    warmup_ratio=0.05,
    bf16=True,
    logging_steps=5,
    eval_strategy="epoch",
    save_strategy="no",
    max_length=2048,
    report_to=[],
)
trainer = SFTTrainer(model=model, args=args, train_dataset=train, eval_dataset=valid, peft_config=lora, processing_class=tok)
trainer.train()
trainer.model.save_pretrained("adapter")  # a PEFT adapter: adapter_config.json and adapter_model.safetensors
print("adapter saved")
"#,
        band = p.band,
        base = p.base,
        hf = p.hf,
        rank = p.rank,
        alpha = p.rank * 2,
        epochs = p.epochs,
        targets = targets,
    )
}

fn lora_yaml(p: &Plan) -> String {
    let keys: String = [
        "self_attn.q_proj",
        "self_attn.k_proj",
        "self_attn.v_proj",
        "self_attn.o_proj",
        "mlp.gate_proj",
        "mlp.up_proj",
        "mlp.down_proj",
    ]
    .iter()
    .map(|k| format!("    - \"{k}\"\n"))
    .collect();
    format!(
        "# Tempo Woodshed: LoRA for the \"{band}\" band with mlx_lm.lora. Read it, change it, run it yourself if you like.\n# The base is {base} (Ollama); mlx_lm trains the 4-bit copy {mlx}.\nmodel: \"{mlx}\"\ntrain: true\ndata: \".\"\nadapter_path: \"adapter\"\nbatch_size: 1\niters: {iters}\nnum_layers: 16\nlearning_rate: 1.0e-5\nlora_parameters:\n  rank: {rank}\n  scale: 20.0\n  dropout: 0.0\n  keys:\n{keys}",
        band = p.band,
        base = p.base,
        mlx = mlx_repo(&p.hf),
        iters = mlx_iters(p.train_examples, p.epochs),
        rank = p.rank,
    )
}

/// mlx_lm writes `adapters.safetensors` in its own layout (`lora_a` as in x r, `lora_b` as r x out);
/// llama.cpp's converter reads a PEFT adapter. This turns one into the other. Never run against a
/// real adapter here: if the names or shapes differ in your mlx_lm, it stops with an error.
const MLX_TO_PEFT: &str = r#"# Turns an mlx_lm adapter folder into a PEFT one, for convert_lora_to_gguf.py.
# usage: python mlx_to_peft.py adapter adapter_peft
import json, sys, os
from safetensors.numpy import load_file, save_file

src, dst = sys.argv[1], sys.argv[2]
cfg = json.load(open(os.path.join(src, "adapter_config.json")))
rank = cfg["lora_parameters"]["rank"]
scale = cfg["lora_parameters"]["scale"]
w = load_file(os.path.join(src, "adapters.safetensors"))
out = {}
for k, v in w.items():
    # model.layers.0.self_attn.q_proj.lora_a  ->  base_model.model.model.layers.0.self_attn.q_proj.lora_A.weight
    if k.endswith(".lora_a"):
        out["base_model.model." + k[: -len(".lora_a")] + ".lora_A.weight"] = v.T.copy()
    elif k.endswith(".lora_b"):
        out["base_model.model." + k[: -len(".lora_b")] + ".lora_B.weight"] = v.T.copy()
    else:
        sys.exit("unexpected tensor name in the mlx adapter: " + k)
os.makedirs(dst, exist_ok=True)
save_file(out, os.path.join(dst, "adapter_model.safetensors"))
targets = sorted({k.split(".")[-3] for k in w if k.endswith(".lora_a")})
json.dump({
    "peft_type": "LORA", "task_type": "CAUSAL_LM", "r": rank, "lora_alpha": scale * rank,
    "lora_dropout": 0.0, "target_modules": targets, "bias": "none",
    "base_model_name_or_path": cfg.get("model", ""),
}, open(os.path.join(dst, "adapter_config.json"), "w"), indent=2)
print("peft adapter written to", dst)
"#;

/// The commands that train and convert, each as an argument list, to run in the work folder.
/// `venv` and `llama_cpp` are folders on the machine that trains.
pub fn steps(p: &Plan, venv: &str, llama_cpp: &str, windows: bool) -> Vec<Vec<String>> {
    let py = venv_bin(venv, "python", windows);
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let mut out = vec![];
    let adapter = match p.backend {
        Backend::Cuda => {
            out.push(s(&[&py, "train.py"]));
            "adapter"
        }
        Backend::Mlx => {
            out.push(s(&[
                &venv_bin(venv, "mlx_lm.lora", windows),
                "--config",
                "lora.yaml",
            ]));
            out.push(s(&[&py, "mlx_to_peft.py", "adapter", "adapter_peft"]));
            "adapter_peft"
        }
    };
    out.push(s(&[
        &py,
        &format!("{llama_cpp}/convert_lora_to_gguf.py"),
        "--base-model-id",
        &p.hf,
        "--outtype",
        "f16",
        "--outfile",
        "adapter.gguf",
        adapter,
    ]));
    out
}

/// What goes into the trainer's own venv, as pip arguments, and a rough size (a guess from what
/// those packages weigh, never measured here).
pub fn pip_packages(b: Backend) -> (Vec<&'static str>, &'static str) {
    match b {
        Backend::Cuda => (
            vec![
                "torch",
                "transformers",
                "peft",
                "trl",
                "bitsandbytes",
                "datasets",
                "accelerate",
                "sentencepiece",
                "numpy",
            ],
            "about 5 GB",
        ),
        // the GGUF converter needs torch too, so a Mac carries it
        Backend::Mlx => (
            vec![
                "mlx-lm",
                "safetensors",
                "numpy",
                "torch",
                "transformers",
                "sentencepiece",
            ],
            "about 3 GB",
        ),
    }
}

/// The commands that make the venv and fill it. `llama_cpp_ready`: its converter is already there.
pub fn install_steps(
    b: Backend,
    venv: &str,
    llama_cpp: &str,
    llama_cpp_ready: bool,
    windows: bool,
) -> Vec<Vec<String>> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let py = venv_bin(venv, "python", windows);
    let mut out = vec![s(&[
        if windows { "python" } else { "python3" },
        "-m",
        "venv",
        venv,
    ])];
    let mut pip = s(&[&py, "-m", "pip", "install"]);
    pip.extend(pip_packages(b).0.iter().map(|x| x.to_string()));
    out.push(pip);
    if !llama_cpp_ready {
        out.push(s(&[
            "git",
            "clone",
            "--depth",
            "1",
            "https://github.com/ggml-org/llama.cpp",
            llama_cpp,
        ]));
    }
    out.push(s(&[
        &py,
        "-m",
        "pip",
        "install",
        "-r",
        &format!("{llama_cpp}/requirements/requirements-convert_lora_to_gguf.txt"),
    ]));
    out
}

/// A command as the owner would type it.
pub fn shown(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(b: Backend) -> Plan {
        Plan {
            band: "social-media".into(),
            backend: b,
            base: "qwen3:8b".into(),
            hf: "Qwen/Qwen3-8B".into(),
            rank: 16,
            epochs: 2,
            train_examples: 85,
        }
    }

    #[test]
    fn hugging_face_names_for_the_families_tempo_knows() {
        assert_eq!(hf_for("qwen3:8b").as_deref(), Some("Qwen/Qwen3-8B"));
        assert_eq!(hf_for("qwen3:0.6b").as_deref(), Some("Qwen/Qwen3-0.6B"));
        assert_eq!(
            hf_for("qwen3:14b-q4_K_M").as_deref(),
            Some("Qwen/Qwen3-14B")
        );
        assert_eq!(
            hf_for("llama3.2:3b").as_deref(),
            Some("meta-llama/Llama-3.2-3B-Instruct")
        );
        assert_eq!(hf_for("gemma3:4b").as_deref(), Some("google/gemma-3-4b-it"));
        for none in ["mistral:7b", "qwen3", "qwen3:latest", "qwen3:x", ""] {
            assert_eq!(hf_for(none), None, "{none}");
        }
        assert_eq!(mlx_repo("Qwen/Qwen3-8B"), "mlx-community/Qwen3-8B-4bit");
    }

    #[test]
    fn the_cuda_script_is_qlora_on_attention_and_mlp_only() {
        let f = files(&plan(Backend::Cuda));
        assert_eq!(f.len(), 1);
        let (name, text) = &f[0];
        assert_eq!(name, "train.py");
        for want in [
            "load_in_4bit=True",
            "r=16,",
            "lora_alpha=32",
            "num_train_epochs=2",
            "train.jsonl",
            "valid.jsonl",
            "\"q_proj\", \"k_proj\", \"v_proj\", \"o_proj\", \"gate_proj\", \"up_proj\", \"down_proj\"",
            "BASE = \"Qwen/Qwen3-8B\"",
            "save_pretrained(\"adapter\")",
        ] {
            assert!(text.contains(want), "{want}");
        }
        // nothing in it reaches out or deletes
        for bad in [
            "rm ",
            "shutil.rmtree",
            "os.remove",
            "requests",
            "urllib",
            "push_to_hub",
        ] {
            assert!(!text.contains(bad), "{bad}");
        }
    }

    #[test]
    fn the_mac_plan_trains_with_mlx_and_converts_the_adapter() {
        let p = plan(Backend::Mlx);
        let f = files(&p);
        let yaml = &f.iter().find(|(n, _)| n == "lora.yaml").unwrap().1;
        assert!(
            yaml.contains("model: \"mlx-community/Qwen3-8B-4bit\"")
                && yaml.contains("iters: 170")
                && yaml.contains("rank: 16")
        );
        assert!(yaml.contains("mlp.down_proj") && yaml.contains("self_attn.q_proj"));
        assert!(f.iter().any(|(n, _)| n == "mlx_to_peft.py"));
        let st = steps(&p, "/w/venv", "/w/llama.cpp", false);
        assert_eq!(st.len(), 3);
        assert_eq!(st[0], ["/w/venv/bin/mlx_lm.lora", "--config", "lora.yaml"]);
        assert_eq!(st[1][1], "mlx_to_peft.py");
        assert_eq!(st[2].last().unwrap(), "adapter_peft");
    }

    #[test]
    fn the_converter_is_llama_cpps_own_script_with_the_flags_its_help_lists() {
        let st = steps(&plan(Backend::Cuda), "/w/venv", "/w/llama.cpp", false);
        assert_eq!(st[0], ["/w/venv/bin/python", "train.py"]);
        assert_eq!(
            st[1],
            [
                "/w/venv/bin/python",
                "/w/llama.cpp/convert_lora_to_gguf.py",
                "--base-model-id",
                "Qwen/Qwen3-8B",
                "--outtype",
                "f16",
                "--outfile",
                "adapter.gguf",
                "adapter"
            ]
        );
        let win = steps(
            &plan(Backend::Cuda),
            "C:\\w\\venv",
            "C:\\w\\llama.cpp",
            true,
        );
        assert_eq!(win[0][0], "C:\\w\\venv\\Scripts\\python.exe");
    }

    #[test]
    fn the_install_makes_its_own_venv_and_says_its_size() {
        let st = install_steps(Backend::Cuda, "/w/venv", "/w/llama.cpp", false, false);
        assert_eq!(st[0], ["python3", "-m", "venv", "/w/venv"]);
        assert!(st[1].contains(&"trl".to_string()) && st[1].contains(&"bitsandbytes".to_string()));
        assert_eq!(st[2][0], "git");
        assert!(st[3]
            .last()
            .unwrap()
            .ends_with("requirements-convert_lora_to_gguf.txt"));
        // llama.cpp already there: no clone
        let st = install_steps(Backend::Mlx, "/w/venv", "/w/llama.cpp", true, false);
        assert!(st.iter().all(|c| c[0] != "git"));
        assert!(pip_packages(Backend::Mlx).0.contains(&"mlx-lm"));
        assert_eq!(pip_packages(Backend::Cuda).1, "about 5 GB");
        assert_eq!(shown(&["a".into(), "b c".into()]), "a \"b c\"");
    }
}
