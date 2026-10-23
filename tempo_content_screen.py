"""Local outbound screen. Returns detector codes, never matched private text."""
from __future__ import annotations
import json
import math
import re
import unicodedata

def normalize(text):
    return ''.join(c for c in unicodedata.normalize('NFKC', text) if unicodedata.category(c) != 'Cf')

def screen(messages, tenant, metadata=None):
    text = normalize('\n'.join(m['content'] for m in messages))
    folded = text.casefold()
    reasons = []
    meta = metadata or {}
    if any(meta.get(k) for k in ('attachments', 'files', 'source_path', 'source_paths', 'private_paths', 'has_private_files')):
        reasons.append('attachment-provenance-unavailable')
    categories = tenant.get('private_categories', [])
    if not isinstance(categories, list) or not all(isinstance(c, str) for c in categories):
        reasons.append('private-category-policy-invalid')
        categories = []
    categories = categories + list(tenant.get('local_only', []))
    if any(normalize(c.replace('-', ' ')).strip().casefold() in folded.replace('-', ' ')
           for c in categories if c.strip()):
        reasons.append('owner-private-category')
    patterns = {
        'birth-date': r'\b(?:dob|date\s+of\s+birth|born)\b\s*[:=]?\s*\d',
        'account-identifier': r'\b(?:account|acct|iban|passport|national\s+id)\b[\s:#=-]{0,20}[a-z0-9][a-z0-9 -]{5,34}',
        'credential': r'\bsk-[a-z0-9_-]{8,}|\bAKIA[A-Z0-9]{16}\b|-----BEGIN [A-Z ]*PRIVATE KEY-----|\b(?:password|api[_ -]?key|secret)\s*[:=]\s*\S{4,}',
        'email': r'\b[a-z0-9.!#$%&\x27*+/=?^_`{|}~-]{1,64}@[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?\.[a-z]{2,63}\b',
        'phone': r'(?<!\w)(?:\+\d{1,3}[ .-]*)?(?:\(\d{3}\)|\d{3})[ .-]*\d{3}[ .-]*\d{4}(?!\d)|(?<!\d)\d{3}[-.]\d{4}(?!\d)',
        # Keep potential PANs local even when mistyped or not Luhn-valid.
        'card-number': r'(?<!\d)\d(?:[ -]?\d){12,18}(?![ -]?\d)',
        'clinical-record': r'\bpatient\s+[a-z][ -]?\d{1,8}\b|\b(?:patient.s\s+(?:treatment|symptom)|unreleased\s+lab|medication\s+reaction|overnight\s+symptom)\b',
    }
    for code, pattern in patterns.items():
        if re.search(pattern, text, re.I): reasons.append(code)
    # Conservative potential-name detector, including Unicode capitals. It can
    # also catch slogans; a pattern hit is kept local, never declassified here.
    previous = None
    for word in re.finditer(r'[^\W\d_]+', text):
        if not (2 <= len(word.group()) <= 31 and word.group().isupper()):
            previous = None
            continue
        if previous and text[previous.end():word.start()].isspace():
            reasons.append('uppercase-name')
            break
        previous = word
    dates = list(re.finditer(r'\b(?:\d{1,2}[/-]\d{1,2}[/-]\d{2,4}|\d{4}-\d{2}-\d{2})\b', text))
    for date in dates:
        area = text[max(0,date.start()-160):date.end()+160]
        if re.search(r'\b(?:[A-Z][a-z]{1,30}\s+[A-Z][a-z]{1,30}|[A-Z]{2,31}\s+[A-Z]{2,31})\b', area):
            reasons.append('name-with-date'); break
    return {'hit': bool(reasons), 'reasons': sorted(set(reasons)), 'decision_model': 'not-run'}

def semantic_screen(messages, tenant, evaluator):
    """All outgoing turns or local fallback. No truncation-to-public allowed."""
    text = normalize(json.dumps(messages, ensure_ascii=False, separators=(',', ':')))
    if len(text) > 32000:
        return {'hit': True, 'reasons': ['semantic-capacity-exceeded'], 'decision_model': 'not-run'}
    chunks = [text[i:i+2000] for i in range(0, len(text), 1800)]
    try:
        for part in chunks:
            verdict = evaluator(part, tenant)
            if verdict is not False:
                return {'hit': True, 'reasons': ['semantic-private' if verdict is True else 'semantic-unavailable'],
                        'decision_model': 'hit' if verdict is True else 'unavailable', 'chunks': len(chunks)}
    except Exception:
        return {'hit': True, 'reasons': ['semantic-unavailable'], 'decision_model': 'unavailable', 'chunks': len(chunks)}
    return {'hit': False, 'reasons': [], 'decision_model': 'pass', 'chunks': len(chunks)}

def decision_evaluator(text, tenant):
    # An arbitrary general-purpose head has no established privacy-screen recall.
    if tenant.get('privacy_screen_reviewed') is not True:
        return None
    threshold=tenant.get('privacy_public_threshold',.99)
    if type(threshold) not in (int,float) or not math.isfinite(threshold) or not .5 <= threshold < 1:
        return None
    from tempo_decision_head import score
    categories = json.dumps(tenant.get('private_categories', []), ensure_ascii=False)
    if len(categories) > 1000:
        return None
    result = score(text, {'type': 'choice', 'instructions': 'Is this text entirely public and free of confidential, personal, client, clinical, legal or owner-private information? Owner private categories: '+categories,
                         'criteria': {'public': 'all text is public', 'private': 'any part is private or uncertain'}})
    probs = result.get('probabilities', {})
    if result.get('predicted_key') == 'public' and probs.get('public', 0) >= threshold:
        return False
    return True
