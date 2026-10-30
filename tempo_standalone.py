"""Built-in single-machine admission. No fleet service, privileged install or cloud call."""
import contextlib
import ctypes
import hashlib
import json
import math
import os
import re
import shutil
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path

class GuardError(RuntimeError):
    pass

def is_standalone(home=None,lease_script=None,cpu_booking=None,cpu_until=0):
    integrated=bool(lease_script or cpu_booking or cpu_until)
    path=Path(home)/'tenant.json' if home else None
    if path and path.exists():
        flag=json.loads(path.read_text(encoding='utf-8')).get('fleet_guard_required',False)
        if type(flag) is not bool:raise GuardError('invalid fleet guard setting')
        if flag and not integrated:raise GuardError('this installation requires its fleet lease or CPU booking')
    return not integrated

class MachineLock:
    """One product inference across processes in this OS instance; nonblocking."""
    def __init__(self,name='PerennosTempo-Inference-v1'):
        self.name=name;self.handle=None
    def __enter__(self):
        if os.name=='nt':
            self.kernel=ctypes.WinDLL('kernel32',use_last_error=True)
            self.kernel.CreateSemaphoreW.argtypes=[ctypes.c_void_p,ctypes.c_long,ctypes.c_long,ctypes.c_wchar_p]
            self.kernel.CreateSemaphoreW.restype=ctypes.c_void_p
            self.kernel.WaitForSingleObject.argtypes=[ctypes.c_void_p,ctypes.c_uint32]
            self.kernel.ReleaseSemaphore.argtypes=[ctypes.c_void_p,ctypes.c_long,ctypes.c_void_p]
            self.kernel.CloseHandle.argtypes=[ctypes.c_void_p]
            h=self.kernel.CreateSemaphoreW(None,1,1,'Global\\'+self.name)
            if not h:raise GuardError('machine lock unavailable; no inference started')
            if self.kernel.WaitForSingleObject(h,0)!=0:
                self.kernel.CloseHandle(h);raise GuardError('machine inference is busy')
            self.handle=h
        else:
            import fcntl
            # Fixed OS path, not a caller's isolated TEMP or per-project folder.
            path='/tmp/perennos-tempo-'+hashlib.sha256(self.name.encode()).hexdigest()[:20]+'.lock'
            try:
                fd=os.open(path,os.O_CREAT|os.O_RDWR|getattr(os,'O_NOFOLLOW',0),0o600)
                try:fcntl.flock(fd,fcntl.LOCK_EX|fcntl.LOCK_NB)
                except OSError:os.close(fd);raise GuardError('machine inference is busy')
                self.handle=fd
            except OSError as exc:raise GuardError('machine lock unavailable; no inference started') from exc
        return self
    def __exit__(self,*args):
        if self.handle is not None:
            if os.name=='nt':self.kernel.ReleaseSemaphore(self.handle,1,None);self.kernel.CloseHandle(self.handle)
            else:
                import fcntl
                fcntl.flock(self.handle,fcntl.LOCK_UN);os.close(self.handle)
            self.handle=None

def free_vram():
    executable=shutil.which('nvidia-smi')
    if not executable:return 0
    try:
        r=subprocess.run([executable,'--query-gpu=memory.free','--format=csv,noheader,nounits'],capture_output=True,text=True,timeout=5)
        if r.returncode:raise GuardError('VRAM probe failed; no inference started')
        values=[int(line.strip()) for line in r.stdout.splitlines() if line.strip()]
        if not values or any(v<0 for v in values):raise ValueError('invalid free VRAM')
        return min(values)  # enough space on every visible GPU; no guessed device mapping
    except (OSError,ValueError,subprocess.TimeoutExpired) as exc:raise GuardError('VRAM probe failed; no inference started') from exc

def required_vram(info,context_mib):
    size=info.get('size')
    if type(size) is not int or size<=0 or not info.get('digest'):raise GuardError('engine size or revision is unknown')
    if type(context_mib) is not int or context_mib<1:raise GuardError('context memory estimate is required')
    return math.ceil(size/1048576)+context_mib+2048

def select_device(info,free_mib,requested='auto',context_mib=2048):
    need=required_vram(info,context_mib)
    if requested not in ('auto','cpu','gpu'):raise GuardError('invalid execution device')
    if requested!='cpu' and free_mib>=need:return 'gpu'
    match=re.fullmatch(r'(\d+(?:\.\d+)?)([BM])',str(info.get('details',{}).get('parameter_size','')))
    parameters=float(match[1])*(1e9 if match[2]=='B' else 1e6) if match else None
    if info['size']>6*1024**3 or parameters is None or parameters>8e9:
        raise GuardError('CPU fallback requires a known small engine (at most 8B and 6 GiB weights)')
    return 'cpu'

_quarantine=[]
class StandaloneGuard:
    def __init__(self,model,url,requested='auto',timeout=180,revision=None,internal_info=None):
        from tempo_cpu import require_loopback
        require_loopback(url)
        if not 1<=timeout<=600:raise GuardError('request timeout must be between 1 and 600 seconds')
        self.model,self.url,self.requested,self.timeout=model,url.rstrip('/'),requested,timeout
        self.revision,self.info=revision,internal_info;self.lock=MachineLock();self.sent=False
        self.context_tokens=8192
    def __enter__(self):
        if _quarantine:raise GuardError('previous inference completion is unknown; restart after checking the runtime is idle')
        self.lock.__enter__()
        try:
            from tempo_local_http import open_local
            if self.info is None:
                busy=json.load(open_local(self.url+'/api/ps',timeout=5)).get('models')
                if busy:raise GuardError('local runtime may already be busy; no inference started')
                tags=json.load(open_local(self.url+'/api/tags',timeout=5))['models']
                self.info=next((m for m in tags if m['name'] in (self.model,self.model+':latest')),None)
                if not self.info:raise GuardError('local engine is not installed')
            if self.revision and self.info.get('digest')!=self.revision:raise GuardError('engine revision changed; setup must check it again')
            self.free=0 if self.requested=='cpu' else free_vram()
            context_mib=2048
            if self.requested!='cpu' and self.free>math.ceil(self.info['size']/1048576)+2048:
                req=urllib.request.Request(self.url+'/api/show',json.dumps({'model':self.model}).encode(),{'Content-Type':'application/json'})
                metadata=json.load(open_local(req,timeout=5)).get('model_info',{})
                arch=metadata.get('general.architecture')
                fields=[metadata.get(str(arch)+'.'+key) for key in ('block_count','embedding_length','attention.head_count','attention.head_count_kv')]
                if any(type(v) is not int or v<=0 for v in fields):raise GuardError('context-memory metadata is unavailable; GPU admission refused')
                layers,width,heads,kv_heads=fields
                context_mib=max(2048,math.ceil(self.context_tokens*layers*(width/heads)*kv_heads*4/1048576))
            self.device=select_device(self.info,self.free,self.requested,context_mib)
            self.deadline=time.monotonic()+self.timeout
            self.receipt={'guard':'standalone','execution_device':self.device,'model_bytes':self.info['size'],
                          'context_mib':context_mib,'context_tokens':self.context_tokens,'reserve_mib':2048,'free_vram_mib':self.free,'timeout_seconds':self.timeout}
            return self
        except Exception:
            self.lock.__exit__(None,None,None);raise
    def dispatched(self):self.sent=True
    def check(self):
        if time.monotonic()>self.deadline:raise GuardError('standalone request timed out')
    def _wait_for_unload(self):
        deadline=min(self.deadline,time.monotonic()+15)
        while True:
            remaining=deadline-time.monotonic()
            if remaining<=0:raise GuardError('local runtime did not become idle before the request deadline')
            from tempo_local_http import open_local
            try:
                # Socket inactivity is capped; the reader also bounds the body by an absolute deadline.
                response=open_local(self.url+'/api/ps',timeout=min(2,remaining))
                from tempo_runtime import read_resident_json
                try:state=read_resident_json(response,deadline)
                finally:response.close()
            except Exception as exc:
                raise GuardError('local runtime idle confirmation failed') from exc
            if time.monotonic()>=deadline:raise GuardError('local runtime idle confirmation exceeded the request deadline')
            if not isinstance(state,dict) or not isinstance(state.get('models'),list):
                raise GuardError('local runtime idle response is invalid')
            if not state['models']:return
            remaining=deadline-time.monotonic()
            if remaining<=0:raise GuardError('local runtime did not become idle before the request deadline')
            time.sleep(min(.25,remaining))
    def __exit__(self,kind,value,traceback):
        if kind is not None and self.sent:
            # The backend's completion is unknown. Retain this process's lock;
            # recovery requires a restart and a fresh idle-runtime check.
            _quarantine.append(self.lock)
        else:
            if self.sent:
                try:self._wait_for_unload()
                except BaseException:
                    _quarantine.append(self.lock)
                    raise
            self.lock.__exit__(kind,value,traceback)

@contextlib.contextmanager
def decision_head_guard(timeout=180):
    from tempo_decision_head import PINS
    info={'size':421000000*4,'digest':PINS['sev_sha256'],'details':{'parameter_size':'421M'}}
    with StandaloneGuard('decision-head','http://127.0.0.1:11434',requested='cpu',timeout=timeout,internal_info=info) as guard:
        yield guard
