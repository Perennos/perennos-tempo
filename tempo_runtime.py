"""Family-specific, per-request Ollama controls. Never edits a model tag."""
import re
import json
import math
import time

def policy(model, family=None):
    name = model.lower()
    family = (family or '').lower()
    if family == 'gpt-oss' or 'gpt-oss' in name:
        return {'family':'gpt-oss','think':'low','wire':'chat'}
    if family == 'qwen3' or re.search(r'(?:^|/)qwen3:',name):
        # The installed Qwen3 template always opens a think block. Supplying its
        # documented empty-think prefix avoids changing that shared model tag.
        return {'family':'qwen3','think':False,'wire':'qwen3-empty-think','soft_switch':'/no_think','final_answer':'validated-json-envelope'}
    if family in {'qwen35','qwen35moe','qwen3next'} or re.search(r'qwen3[.5-]',name):
        return {'family':family or 'qwen3','think':False,'wire':'chat'}
    return {'family':family or 'other','wire':'chat'}

def _resident_options(config, witness, model, family, cpu):
    """Delegate owner-only resident checks; public bundles omit the adapter."""
    try:
        from tempo_owner_resident import _resident_options as validate
    except ModuleNotFoundError as exc:
        if exc.name != 'tempo_owner_resident':
            raise
        raise ValueError('resident backend adapter unavailable') from None
    return validate(config, witness, model, family, cpu)


def request(model, messages, temperature, max_tokens, *, cpu=False, family=None, stream=True, final_only=True, resident_policy=None, backend_witness=None, release_after=False):
    resident = None
    if resident_policy is not None or backend_witness is not None:
        resident = _resident_options(resident_policy,backend_witness,model,family,cpu)
    controls = policy(model,family)
    body = {'model':model,'stream':stream,'options':{'temperature':temperature,'num_predict':max_tokens}}
    if 'think' in controls: body['think'] = controls['think']
    if cpu or release_after:
        body['keep_alive'] = -1 if resident is not None else 0
    if cpu:
        body['options'].update(resident if resident is not None else {'num_gpu':0,'num_thread':2,'num_ctx':8192})
    if controls['wire']=='qwen3-empty-think':
        last_user=max((i for i,m in enumerate(messages) if m['role']=='user'),default=-1)
        prompt=''.join('<|im_start|>'+m['role']+'\n'+m['content']+('\n/no_think' if i==last_user else '')+'<|im_end|>\n' for i,m in enumerate(messages))
        if final_only:
            prompt='<|im_start|>system\nReturn one JSON object with an answer string containing ONLY the final answer requested by the user. Never include analysis, reasoning, planning, or discussion of the instructions in the answer. Keep the requested wording and length.<|im_end|>\n'+prompt
            body['format']={'type':'object','properties':{'answer':{'type':'string'}},'required':['answer'],'additionalProperties':False}
        body.update(raw=True,prompt=prompt+'<|im_start|>assistant\n<think>\n\n</think>\n\n')
        return '/api/generate',body
    body['messages'] = messages
    return '/api/chat',body

def read_resident_json(response,deadline):
    """Bound a nonstreamed resident response by bytes and an absolute deadline."""
    chunks=[];size=0
    while True:
        remaining=deadline-time.monotonic()
        if remaining<=0:raise TimeoutError('resident qualification deadline exceeded')
        sock=getattr(getattr(getattr(response,'fp',None),'raw',None),'_sock',None)
        if sock is not None:sock.settimeout(remaining)
        read=getattr(response,'read1',None)
        part=read(min(16384,1048577-size)) if callable(read) else response.read(1)
        if time.monotonic()>=deadline:raise TimeoutError('resident qualification deadline exceeded')
        if not part:break
        if type(part) is not bytes:raise RuntimeError('invalid resident response bytes')
        size+=len(part)
        if size>1048576:raise RuntimeError('resident response exceeds byte bound')
        chunks.append(part)
    value=json.loads(b''.join(chunks))
    if not isinstance(value,dict):raise RuntimeError('invalid resident response object')
    return value


def begin_resident_request(config,provider,model,family,digest,cpu_booking,cpu_until,timeout):
    """Bind a trusted runner snapshot to the qualified engine and caller budget."""
    import copy
    if not callable(provider) or type(config) is not dict or config.get('digest')!=digest:
        raise ValueError('resident backend differs from qualified engine')
    witness=copy.deepcopy(provider())
    _resident_options(config,witness,model,family,True)
    if (witness['cpu_booking']!=cpu_booking or type(cpu_until) not in (int,float)
            or type(timeout) not in (int,float) or not math.isfinite(timeout) or timeout<=0
            or not math.isfinite(cpu_until) or witness['valid_until']>cpu_until
            or witness['valid_until']-time.time()<timeout):
        raise ValueError('resident request does not fit caller resource window')
    return witness


def confirm_resident_backend(config,before,after,model,family):
    """Check fresh completion metadata against this request's initial witness."""
    _resident_options(config,after,model,family,True)
    for key in ('runner_pid','runner_birth_id','shared_fence_id','cpu_booking','ram_booking','model_booking'):
        if after[key]!=before[key]:
            raise ValueError('resident runner or request resource identity changed')


def visible_answer(text,model,family=None,*,require_envelope=False):
    text=text.strip()
    if policy(model,family).get('think') is False:
        if '</think>' in text:text=text.rsplit('</think>',1)[1].strip()
        if '<think>' in text:raise RuntimeError('unclosed reasoning response')
    if not text:raise RuntimeError('empty answer after reasoning removal')
    if require_envelope:
        try:value=json.loads(text)
        except ValueError as exc:raise RuntimeError('invalid final-answer envelope; no answer returned') from exc
        if not isinstance(value,dict) or set(value)!= {'answer'} or not isinstance(value['answer'],str):
            raise RuntimeError('invalid final-answer envelope; no answer returned')
        text=value['answer'].strip()
        if not text or '<think>' in text or '</think>' in text:raise RuntimeError('empty or reasoning-only final-answer envelope')
    return text
