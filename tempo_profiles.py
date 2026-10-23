import json
import math
import re
from pathlib import Path
from typing import Any, Dict, Tuple

DEFAULT_WORKFLOWS = {'design-studio':'design-studio'}

def defaults() -> Dict[str, Any]:
    """Return the default tempo profile configuration."""
    return {
        "version": 1,
        "default_profile": "private",
        "workflows": dict(DEFAULT_WORKFLOWS),
        "profiles": [
            {
                "name": "private",
                "label": "Perennos Tempo Private",
                "hosted_allowed_classes": [],
                "preferred": {},
                "uncensored": False,
                "exploration": False,
                "max_cost_per_job": 0,
                "requires_review": True,
            },
            {
                "name": "design-studio",
                "label": "Perennos Tempo Design Studio",
                "hosted_allowed_classes": ["design", "brand", "copy", "imagery-brief", "ui", "storyboard", "public-research"],
                "preferred": {},
                "uncensored": False,
                "exploration": False,
                "max_cost_per_job": 0,
                "requires_review": True,
            },
            {
                "name": "frontier-assist",
                "label": "Perennos Tempo Frontier Assist",
                "hosted_allowed_classes": ["public-research", "code", "public-draft"],
                "preferred": {},
                "uncensored": False,
                "exploration": False,
                "max_cost_per_job": 0,
                "requires_review": True,
            },
            {
                "name": "offline",
                "label": "Perennos Tempo Offline",
                "hosted_allowed_classes": [],
                "preferred": {},
                "uncensored": False,
                "exploration": False,
                "max_cost_per_job": 0,
                "requires_review": True,
            },
            {
                "name": "custom",
                "label": "Perennos Tempo Custom",
                "hosted_allowed_classes": [],
                "preferred": {},
                "uncensored": False,
                "exploration": False,
                "max_cost_per_job": 0,
                "requires_review": True,
            },
        ],
    }


def load(data_dir: str | Path) -> Dict[str, Any]:
    """Load the tempo profile configuration from a JSON file."""
    path = Path(data_dir) / "profiles.json"
    data = json.loads(path.read_text(encoding="utf-8-sig")) if path.exists() else defaults()
    return validate(data)


def validate(data):
    if not isinstance(data, dict) or data.get('version') != 1 or not isinstance(data.get('profiles'), list):
        raise ValueError('invalid profiles schema')
    names = set()
    for p in data['profiles']:
        if not isinstance(p, dict) or not isinstance(p.get('name'), str) or not re.fullmatch(r'[a-z][a-z0-9-]{0,39}', p['name']):
            raise ValueError('invalid profile name')
        if p['name'] in names or p['name'] in {'auto','fast','heavy','writer','research'}:
            raise ValueError('duplicate or reserved profile name')
        names.add(p['name'])
        if not isinstance(p.get('label'), str) or not 1 <= len(p['label']) <= 120:
            raise ValueError('invalid profile label')
        cap = p.get('max_cost_per_job')
        if type(cap) not in (int,float) or not math.isfinite(cap) or cap < 0:
            raise ValueError('profile cap must be finite and nonnegative')
        classes = p.get('hosted_allowed_classes')
        if classes not in ('none','*') and not (isinstance(classes,list) and all(isinstance(c,str) and re.fullmatch(r'[a-z][a-z0-9-]{0,39}',c) for c in classes)):
            raise ValueError('invalid hosted classes')
        if p['name'] in {'private','offline'} and classes not in ('none',[]):
            raise ValueError('Private and Offline cannot permit hosted work')
        if any(type(p.get(k)) is not bool for k in ('uncensored','exploration','requires_review')):
            raise ValueError('profile switches must be booleans')
        preferred = p.get('preferred')
        if not isinstance(preferred,dict) or any(k not in {'thinking','research','inference','writing-code','imagery','video'} for k in preferred):
            raise ValueError('invalid preferred roles')
        for ids in preferred.values():
            if not isinstance(ids,(str,list)) or not all(isinstance(x,str) and 0 < len(x) <= 128 for x in ([ids] if isinstance(ids,str) else ids)):
                raise ValueError('invalid preferred engines')
    if not {'private','design-studio','frontier-assist','offline','custom'} <= names:
        raise ValueError('built-in profiles must remain available')
    if data.get('default_profile') not in names:
        raise ValueError('unknown tenant default profile')
    if not isinstance(data.get('workflows'),dict) or any(not isinstance(k,str) or not k or v not in names for k,v in data['workflows'].items()):
        raise ValueError('invalid workflow profile defaults')
    return data


def resolve(
    store: Dict[str, Any], model: str, workflow: str | None = None, privacy_class: str | None = None
) -> Tuple[Dict[str, Any], str]:
    """Resolve a tempo profile and return the profile dict and its source."""
    if not isinstance(model,str) or not model.startswith('tempo/'):
        raise ValueError('invalid profile model')
    legacy={'tempo/auto','tempo/fast','tempo/heavy','tempo/writer','tempo/research'}
    workflows = {**DEFAULT_WORKFLOWS, **store.get('workflows',{})}
    if model not in legacy:
        name,source=model[6:],'explicit'
    elif privacy_class == 'public' and workflow in workflows:
        name,source=workflows[workflow],'workflow'
    else:
        name,source=store['default_profile'],'tenant'
    profile = next((p for p in store["profiles"] if p["name"] == name), None)
    if profile is None:
        raise ValueError(f"Profile '{name}' not found in store")
    return profile, source


def is_private(route: Dict[str, Any], tenant: Dict[str, Any]) -> bool:
    """Determine if a route is private."""
    immutable = {
        "private",
        "family",
        "legal",
        "medical",
        "patient-records",
        "client-files",
        "kyc",
        "student-records",
        "trust-deed",
        "client-letter",
    }
    local_only=tenant.get('local_only',[])
    if not isinstance(local_only,list) or not all(isinstance(c,str) for c in local_only):
        raise ValueError('invalid owner privacy classes')
    immutable |= {re.sub(r'[\s_]+','-',c.strip().lower()) for c in local_only}
    meta=route.get('metadata') or {}
    labels={re.sub(r'[\s_]+','-',str(route.get(k,'')).strip().lower()) for k in ('task_class','privacy_class')} | {re.sub(r'[\s_]+','-',str(meta.get(k,'')).strip().lower()) for k in ('task_class','privacy_class')}
    if tenant.get("vertical") == "fiduciary" or immutable & labels:
        return True
    return route.get('privacy_class',meta.get('privacy_class','private')) != 'public'


def hosted_allowed(route: Dict[str, Any], profile: Dict[str, Any], tenant: Dict[str, Any]) -> bool:
    """Return True if the route can be hosted under the given profile."""
    if is_private(route, tenant):
        return False
    if profile["name"] in ("private", "offline"):
        return False
    task_class = route.get('task_class') or route.get('metadata', {}).get('task_class')
    allowed=profile.get('hosted_allowed_classes',[])
    return allowed == '*' or isinstance(allowed,list) and task_class in allowed
