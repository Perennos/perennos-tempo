"""Per-request profile boundary around the E1 local gateway."""
from __future__ import annotations
import hashlib
import json
import time
import uuid
from pathlib import Path
import tempo_content_screen as content_screen
import tempo_hosted
from tempo_receipts import append_receipt

class ProfileGateway:
    def __init__(self, local, semantic_evaluator=None, endpoint_overrides=None):
        self.local = local
        self.semantic_evaluator = semantic_evaluator or content_screen.decision_evaluator
        self.endpoint_overrides = endpoint_overrides
        import tempo_profiles
        path = self.data_dir / 'profiles.json'
        if not path.exists():
            self.data_dir.mkdir(parents=True, exist_ok=True)
            try:
                with path.open('x', encoding='utf-8') as stream:
                    json.dump(tempo_profiles.defaults(), stream, indent=2)
            except FileExistsError:
                pass
        tempo_profiles.load(self.data_dir)  # malformed policy refuses startup

    def __getattr__(self, name):
        return getattr(self.local, name)

    def tenant(self):
        path = self.data_dir / 'tenant.json'
        obj = json.loads(path.read_text(encoding='utf-8-sig')) if path.exists() else {}
        if not isinstance(obj, dict): raise ValueError('invalid tenant policy')
        return obj

    def models(self):
        import tempo_profiles
        store = tempo_profiles.load(self.data_dir)
        return [{'id': 'tempo/'+p['name'], 'object': 'model', 'owned_by': 'perennos-tempo',
                 'name': p['label'], 'description': 'Local only' if p['hosted_allowed_classes'] in ('none', []) else 'Cloud allowed only after privacy gates'}
                for p in store['profiles']]

    def _boundary(self, route, tenant, profile, screen):
        from tempo_verdicts import ConversationTracker
        import tempo_profiles
        tracker = None
        sticky = False
        if route.get('conversation_id'):
            key = hashlib.sha256((str(tenant.get('tenant','default'))+':'+route['conversation_id']).encode()).hexdigest()
            tracker = ConversationTracker(self.ledger/'profile-boundaries', key)
            tracker.acquire()
        try:
            sticky = (tracker.load() or {}).get('privacy_class') == 'private' if tracker else False
            private = sticky or tempo_profiles.is_private(route, tenant) or profile['name'] in ('private','offline') or screen['hit']
            if tracker and private: tracker.save({'privacy_class': 'private', 'attempt_id':'boundary-'+uuid.uuid4().hex})
        except Exception:
            if tracker: tracker.release()
            raise
        return tracker, sticky

    def run(self, route, on_stream=None, on_start=None, client_socket=None):
        import tempo_profiles
        import tempo_ratings
        tenant = self.tenant()
        store = tempo_profiles.load(self.data_dir)
        profile, source = tempo_profiles.resolve(store, route['model'], route.get('metadata',{}).get('workflow'), route['privacy_class'])
        screened = content_screen.screen(route['messages'], tenant, route.get('metadata'))
        tracker, sticky = self._boundary(route, tenant, profile, screened)
        verdict_tracker = None
        context = {'profile': profile['name'], 'profile_source': source, 'requires_review': profile['requires_review'] or tempo_profiles.is_private(route,tenant),
                   'screen_result': screened, 'hosted': False, 'route_reason': 'profile-local',
                   'profile_policy_sha256':hashlib.sha256(json.dumps(profile,sort_keys=True).encode()).hexdigest()}
        job_id = 'chatcmpl-'+uuid.uuid4().hex
        route = dict(route, _job_id=job_id)
        def local(reason):
            context['route_reason'] = reason
            from urllib.parse import urlsplit
            import ipaddress
            endpoint=urlsplit(self.ollama_url)
            if endpoint.scheme not in {'http','https'} or endpoint.username or endpoint.password or endpoint.query or endpoint.fragment:
                raise ValueError('invalid local inference endpoint')
            host=endpoint.hostname
            if host not in {'127.0.0.1','localhost','::1'}:
                try: address=ipaddress.ip_address(host)
                except ValueError: raise ValueError('local inference needs a loopback or explicitly approved private IP')
                networks=[ipaddress.ip_network(n) for n in ('10.0.0.0/8','172.16.0.0/12','192.168.0.0/16','fc00::/7')]
                if host not in tenant.get('approved_local_hosts',[]) or not any(address.version==n.version and address in n for n in networks):
                    raise ValueError('local inference endpoint is not approved private infrastructure')
            if profile['name']=='offline':
                if host not in {'127.0.0.1','localhost','::1'}:
                    raise ValueError('Offline profile requires loopback local inference')
            append_receipt(self.ledger/'receipts.jsonl',{'job_id':job_id,'status':'routed_local',
                'task_class':route['task_class'],'privacy_class':route['privacy_class'],**context})
            # Preserve the normal E1 streaming and disconnect behavior.
            result = self.local.run(dict(route, _profile_context=context, _profile_policy=profile), on_stream=on_stream,
                                    on_start=on_start, client_socket=client_socket)
            result['tempo'] = {'profile': profile['name'], 'review_before_use': context['requires_review'],
                               'kept_local_reason': reason, 'screen': context['screen_result']}
            return result
        try:
            if sticky or screened['hit'] or not tempo_profiles.hosted_allowed(route, profile, tenant):
                return local('private-thread' if sticky else 'content-screen' if screened['hit'] else 'privacy-or-profile')
            registry = json.loads((self.data_dir/'engines.json').read_text(encoding='utf-8'))['engines']
            candidates=[]
            for engine in registry:
                if engine.get('locality')!='hosted' or engine.get('qualified') is not True or route['role'] not in engine.get('roles',[]): continue
                if not profile['uncensored'] and (engine.get('uncensored') or any(x in engine.get('model','').lower() for x in ('uncensored','abliterat','heretic','dolphin'))): continue
                if not tempo_ratings.profile_eligible(route, engine, tenant, profile): continue
                try:
                    tempo_hosted.credential(engine, tenant)
                    reserve=tempo_hosted.reserve(engine, profile, route['messages'], route['max_tokens'])
                except ValueError: continue
                candidates.append((engine, reserve))
            if not candidates: return local('no-approved-hosted-engine-within-cap')
            if self.semantic_evaluator is content_screen.decision_evaluator and tenant.get('privacy_screen_reviewed') is not True:
                context['screen_result']={'hit':True,'reasons':['semantic-screen-unreviewed'],'decision_model':'unavailable'}
                return local('semantic-screen-unreviewed')
            preferred=profile.get('preferred',{}).get(route['role'],[])
            if isinstance(preferred,str): preferred=[preferred]
            candidates.sort(key=lambda x:(preferred.index(x[0]['id']) if x[0]['id'] in preferred else len(preferred),
                                           int(x[0].get('policy_order',0)),x[0]['id']))
            engine, reserve=candidates[0]
            holder='tempo-profile-screen-'+uuid.uuid4().hex[:12]
            leased=False
            try:
                self.local._lease(holder); leased=True
                screened=content_screen.semantic_screen(route['messages'],tenant,self.semantic_evaluator)
            except Exception:
                screened={'hit':True,'reasons':['semantic-unavailable'],'decision_model':'unavailable'}
            finally:
                if leased: self.local._release(holder)
            context['screen_result']=screened
            if screened['hit']:
                if tracker: tracker.save({'privacy_class':'private', 'attempt_id':'boundary-'+uuid.uuid4().hex})
                return local('semantic-screen')
            # Independent final boundary: reload owner policy, re-screen all text, verify attachment and cap.
            fresh_tenant=self.tenant()
            fresh_profile,_=tempo_profiles.resolve(tempo_profiles.load(self.data_dir),route['model'],route.get('metadata',{}).get('workflow'),route['privacy_class'])
            final_screen=content_screen.screen(route['messages'],fresh_tenant,route.get('metadata'))
            if final_screen['hit'] or not tempo_profiles.hosted_allowed(route,fresh_profile,fresh_tenant):
                context['screen_result']=final_screen
                return local('final-privacy-gate')
            current=json.loads((self.data_dir/'engines.json').read_text(encoding='utf-8'))['engines']
            if engine not in current or not tempo_ratings.profile_eligible(route,engine,fresh_tenant,fresh_profile):
                return local('engine-policy-changed')
            reserve=tempo_hosted.reserve(engine,fresh_profile,route['messages'],route['max_tokens'])
            tempo_hosted.credential(engine,fresh_tenant)
            context.update(hosted=True,route_reason='explicit-public-profile-and-screens-passed',requires_review=True)
            attempt=uuid.uuid4().hex
            from tempo_gateway import _sha, _canonical
            request_hash = _sha(_canonical({'messages':route['messages'], 'task_class':route['task_class'],
                                           'role':route['role'], 'privacy_class':route['privacy_class']}))
            verdict_tracker, conversation = self.local._transition(route, request_hash)
            common={'job_id':job_id,'attempt_id':attempt,'task_class':route['task_class'],'privacy_class':route['privacy_class'],
                    'role':route['role'],'engine_id':engine['id'],'request_sha256':request_hash,**context,**reserve}
            started=append_receipt(self.ledger/'receipts.jsonl',{**common,'status':'started'})
            if on_start: on_start(started,job_id,engine['model'])
            try:
                text,usage=tempo_hosted.complete(engine,fresh_tenant,route,self.endpoint_overrides,self.timeout)
            except Exception as exc:
                append_receipt(self.ledger/'receipts.jsonl',{**common,'status':'unknown','error_code':type(exc).__name__,
                               'cost_status':'reserved-outcome-unknown'})
                raise RuntimeError('hosted outcome unknown; no automatic retry') from None
            if on_stream: on_stream(text,job_id,engine['model'])
            actual=None
            if all(k in usage for k in ('prompt_tokens','completion_tokens')):
                prices=engine['price_per_million']
                actual=(usage['prompt_tokens']*float(prices['input'])+usage['completion_tokens']*float(prices['output']))/1e6
            ended=append_receipt(self.ledger/'receipts.jsonl',{**common,'status':'completed',
                'response_sha256':hashlib.sha256(text.encode()).hexdigest(),**usage,'actual_usd':actual,
                'cost_status':'provider-reported' if actual is not None else 'usage-unavailable-reserved'})
            from tempo_store import append_jsonl
            append_jsonl(self.ledger/'attempts.jsonl',{'kind':'tempo.attempt.v1',**common,'at':started['recorded_at'],'completed_at':ended['recorded_at'],
                'engine_revision':engine.get('revision'),'response_sha256':ended['response_sha256'],**usage,'actual_usd':actual})
            if verdict_tracker:
                verdict_tracker.save({'attempt_id':attempt, 'request_sha256':request_hash,
                    'response_sha256':ended['response_sha256'], 'feedback_sha256':conversation['feedback_sha256'],
                    'completed_at':ended['recorded_at'], 'task_class':route['task_class'], 'role':route['role'],
                    'privacy_class':route['privacy_class'], 'engine_id':engine['id'], 'revision':engine.get('revision')})
            return {'id':job_id,'object':'chat.completion','created':int(time.time()),'model':engine['model'],
                'choices':[{'index':0,'message':{'role':'assistant','content':text},'finish_reason':'stop'}], 'usage':usage,
                'tempo':{'profile':profile['name'],'review_before_use':True,'screen':screened},
                '_tempo':{'job_id':job_id,'receipt_id':started['receipt_id'],'completion_receipt_id':ended['receipt_id']}}
        finally:
            if verdict_tracker: verdict_tracker.release()
            if tracker: tracker.release()
