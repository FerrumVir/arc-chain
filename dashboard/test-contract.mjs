#!/usr/bin/env node

import {readFileSync} from 'node:fs';

const dashboardUrl=new URL('./index.html',import.meta.url);
const html=readFileSync(dashboardUrl,'utf8');
const css=readFileSync(new URL('./tailwind.css',import.meta.url),'utf8');
let passed=0;

function check(condition,message){
  if(!condition){
    throw new Error(`dashboard contract failed: ${message}`);
  }
  passed+=1;
}

check(html.startsWith('<!DOCTYPE html>'),'index.html must remain a standalone HTML document');
check(html.includes('href="./tailwind.css"'),'dashboard must load its compiled local stylesheet');
check(!html.includes('cdn.tailwindcss.com'),'production dashboard must not execute the Tailwind development CDN');
check(css.includes('.text-arc-300'),'compiled CSS must contain the extended ARC palette');
check(css.includes('.bg-surface-2'),'compiled CSS must contain the custom surface palette');

// The active topology must have one source of truth and exactly the six seed
// hosts supported by the dashboard. This catches the retired-host and "8 node"
// regressions that previously left dead cards and misleading copy in production.
const topologyMatch=html.match(/const NODES=Object\.freeze\(\[([\s\S]*?)\]\);/);
check(topologyMatch,'NODES must be a frozen, explicit topology inventory');
const configuredHosts=[...topologyMatch[1].matchAll(/ip:'([^']+)'/g)].map(match=>match[1]).sort();
const expectedHosts=[
  '104.238.171.11',
  '136.244.109.1',
  '140.82.16.112',
  '149.28.153.31',
  '149.28.32.76',
  '202.182.107.41',
].sort();
check(JSON.stringify(configuredHosts)===JSON.stringify(expectedHosts),'topology must contain exactly the six active seed hosts');
check((html.match(/const NODES=/g)||[]).length===1,'topology must not be duplicated');
check(html.includes('const RPC_FALLBACKS=NODES.map('),'RPC fallbacks must derive from NODES');
check(html.includes('const GATEWAY_SEEDS=NODES.map('),'scoreboard hosts must derive from NODES');

const forbiddenTopology=[
  [/(?:216\.238\.120\.27|139\.84\.237\.49)/,'retired seed IP'],
  [/(?:149\.28\.32\.76:10000|:3001|\/community\/list)/,'obsolete inference/community endpoint'],
  [/(?:\ball\s+8\b|\b8\s+(?:nodes|seeds?|devices?)\b|Run on All 8)/i,'obsolete eight-host claim'],
  [/(?:6 continents|6\.2×|0(?:\.00)?% quality loss)/i,'hard-coded or false dashboard metric'],
];
for(const [pattern,label] of forbiddenTopology){
  check(!pattern.test(html),`must not contain ${label}`);
}

// Scoreboard contract: success_count is canonical, while work_completed is a
// compatibility fallback. Rendering and aggregation must share one accessor.
check(html.includes('/workers/scoreboard'),'community workers must use the read-only scoreboard endpoint');
check(html.includes('worker.success_count??worker.work_completed??0'),'completed jobs must prefer success_count');
check(html.includes('const work=completedJobsOf(worker);'),'worker cards must use the same completed-job accessor');
check(!/const work\s*=\s*w\.work_completed\s*\|\|/.test(html),'worker cards must not ignore success_count');

// Community inference must use the worker-dispatching route on the selected
// coordinator. The sharded demo may keep run_consensus, but this action may not.
const communityRenderStart=html.indexOf('function renderCommunityInferenceResult(');
const communityRunStart=html.indexOf('async function runCommunityInference(');
const communityWorkersStart=html.indexOf('// ── Community workers',communityRunStart);
check(communityRenderStart>=0&&communityRunStart>communityRenderStart,'community inference renderer and action must exist');
check(communityWorkersStart>communityRunStart,'community inference action must have a bounded source section');
const communityRenderSource=html.slice(communityRenderStart,communityRunStart);
const communityRunSource=html.slice(communityRunStart,communityWorkersStart);
check(communityRunSource.includes("fetch(SHARDED_COORDINATOR+'/inference/run'"),'community action must POST /inference/run on the selected coordinator');
check(communityRunSource.includes("method:'POST'"),'community inference must use POST');
check(!communityRunSource.includes('/inference/run_consensus'),'community action must not use the seed-only consensus demo endpoint');
check(communityRunSource.includes('max_tokens:8'),'one-click community work must use a bounded token request');
check(html.includes("communityInferenceBtn')?.addEventListener('click',runCommunityInference)"),'community inference button must have a one-click action');
check(communityRenderSource.includes('payload.routed_via'),'community result must display routed_via');
check(communityRenderSource.includes('payload.verification'),'community result must display coordinator verification evidence');
check(communityRenderSource.includes('payload.settlement'),'community result must display settlement status');
check(communityRenderSource.includes("verification.method??'unavailable'"),'verification method must stay unavailable when absent');
check(communityRenderSource.includes("verification.range_position_quorums??'unavailable'"),'quorum records must not be mislabeled as raw votes');
check(communityRenderSource.includes("verification.signatures_required_per_quorum??'unavailable'"),'verification must disclose signatures required per quorum');
check(communityRenderSource.includes('Coordinator quorum summary'),'verification card must not imply that discarded signatures are a client-verifiable proof bundle');
check(communityRenderSource.includes("settlement.status??'unavailable'"),'settlement status must stay unavailable when absent');
check(communityRenderSource.includes('resultEl.replaceChildren()'),'community results must rebuild with DOM nodes');
check(communityRenderSource.includes('makeElement('),'community results must render remote values through textContent-backed elements');
check(!communityRenderSource.includes('.innerHTML'),'community results must never render remote or prompt data through innerHTML');
check(communityRunSource.includes('status.textContent'),'community request status must render coordinator data as text');
check(!communityRunSource.includes('.innerHTML'),'community request status must never render coordinator data through innerHTML');
check(communityRenderSource.includes('settlement.included===true&&!negativeStatus'),'settlement inclusion must not override a negative or pending status');
check(communityRenderSource.includes('submitted|pending|rejected|failed|unavailable|not_activated|missing'),'settlement guards must classify pending and rejected rewards as unearned');
check(communityRenderSource.includes('Pending or rejected')||html.includes('Pending or rejected rewards are never counted as earned.'),'community UI must explicitly reject pending/rejected rewards as earnings');

// Each deduplicated scoreboard row must retain its source seed, and its
// on-chain earnings must be fetched from that same seed. Only mined-success
// totals are shown as earned; null rate/balance data remains unavailable.
const workerRefreshStart=html.indexOf('async function refreshCommunityWorkers(');
const workerRefreshEnd=html.indexOf('// Start community actions and worker polling.',workerRefreshStart);
check(workerRefreshStart>=0&&workerRefreshEnd>workerRefreshStart,'community worker refresh implementation must exist');
const workerRefreshSource=html.slice(workerRefreshStart,workerRefreshEnd);
check(workerRefreshSource.includes('seen.set(workerId,{worker,sourceSeed,sourceName})'),'deduplicated worker records must retain their scoreboard source seed');
check(workerRefreshSource.includes('`${record.sourceSeed}/worker/earnings/${encodeURIComponent(workerId)}`'),'worker earnings must come from the same seed as the selected scoreboard record');
check(workerRefreshSource.includes('earnings?.onchain_balance_arc'),'worker cards must show actual on-chain ARC balance');
check(workerRefreshSource.includes('earnings?.total_rewards'),'worker cards must use the successful mined reward count');
check(workerRefreshSource.includes('Mined rewards visible here'),'reward count must disclose retained visibility scope');
check(!/total_rewards\s*(?:\?\?|\|\|)\s*0/.test(workerRefreshSource),'missing mined reward data must not be converted to zero');
check(!workerRefreshSource.includes('estimated_total_arc'),'estimated gross history must not be presented as actual earnings');
check(workerRefreshSource.includes('earnings?.community_rewards_v1_enabled'),'worker cards must show end-to-end reward readiness');
check(workerRefreshSource.includes('earnings?.community_rewards_v1_protocol_active'),'worker cards must distinguish protocol activation from issuance readiness');
check(workerRefreshSource.includes('earnings?.community_rewards_v1_approval_collection_ready'),'worker cards must consume validator approval-collection readiness');
check(workerRefreshSource.includes('Blocked: validator approval collection unavailable'),'worker cards must explain fail-closed threshold issuance');
check(workerRefreshSource.includes('earnings?.attestations_per_day_observed'),'worker cards must show the observed rate when available');
check(workerRefreshSource.includes('backward-looking')&&workerRefreshSource.includes('not guaranteed'),'rate display must say it is backward-looking and not guaranteed');
check(workerRefreshSource.includes('sourceSeed===SHARDED_COORDINATOR'),'capacity must be scoped to the coordinator that receives the action');
check(workerRefreshSource.includes('data?.eligible_inference_workers'),'capacity must come from the coordinator’s exact server-side eligibility check');
check(workerRefreshSource.includes("worker.model_id.trim().replace(/^0x/i,'').toLowerCase()"),'worker compatibility must compare exact normalized model identities');
check(workerRefreshSource.includes("capabilities.includes('inference')&&workerModelId===coordinatorModelId"),'compatible cards must require capability plus exact model identity');
check(workerRefreshSource.includes('Network-visible registrations are not proof'),'missing eligibility metadata must remain unknown rather than inferred');
check(workerRefreshSource.includes("'observer · no model'"),'worker cards must explain why a model-less observer cannot claim inference');
check(workerRefreshSource.includes('this action will fall back locally'),'zero coordinator-compatible capacity must predict the honest fallback');
check(!workerRefreshSource.includes('.innerHTML'),'worker and earnings records must render through safe DOM nodes');
check(html.includes("if(value===null||value===undefined||value==='')return null;"),'numeric helpers must preserve null as unavailable');

// Public copy must not send operators back to the legacy main-branch
// curl|bash installer or overstate what old shape-derived model IDs prove.
check(html.includes('releases/download/v0.7.12/install.sh'),'headless CTA must pin the recovery release installer');
check(html.includes('bash install.sh --version 0.7.12 --model'),'headless CTA must pin the installed node version');
check(!html.includes('install-community-node.sh | bash'),'headless CTA must not use the legacy unchecked installer');
check(!html.includes('arc-demo.sh | bash'),'dashboard must not advertise an unchecked live-fleet demo one-liner');
check(html.includes('agreement is not proof of identical weight bytes'),'legacy model ID agreement must not be presented as exact artifact verification');
check(html.includes('Selected host live - height'),'header liveness must remain scoped to the selected RPC host');
check(!html.includes('~3 minutes from curl to running node'),'dashboard must not promise a model-backed install duration');
check(!html.includes('Selected host live - block'),'header must not mislabel a DAG round as a block');
check(html.includes('DAG commits (process)'),'DAG commit counter must disclose its process-local scope');
check(!html.includes('>Finalized<'),'process-local DAG commits must not be labeled finalized blocks');
check(!html.includes('✗ OOM')&&!html.includes("Won't load."),'memory estimate must not promise an unmeasured OOM');
check(!html.includes('Loads everywhere.'),'shard memory estimate must not guarantee host compatibility');
check(!html.includes('O(1) lookup (~20 μs)'),'cache UI must not show an unmeasured latency constant');
check(html.includes('id="fleetConsistency"'),'dashboard must expose a fleet-consistency status');
check(html.includes('LARGE FLEET DIVERGENCE'),'large seed height drift must be surfaced explicitly');
check(html.includes('Do not treat them as one canonical chain'),'height divergence must stop canonical-chain marketing claims');
check(html.includes('COMMON-HEIGHT FORK CONFIRMED'),'same-height hash/root disagreement must be surfaced explicitly');
check(html.includes('auditFleetAtCommonHeight'),'dashboard must compare commitments at one common height');
check(html.includes('new Set(valid.map(sample=>sample.root)).size'),'root comparison must count distinct exact commitments');
check(html.includes('Stop reward issuance and choose one canonical recovery state'),'confirmed fork must provide an operational stop condition');
check(html.includes("liveness=d.chain_advancing===true?'advancing':d.chain_advancing===false?'stalled':'unknown'"),'reachability must not be mislabeled as chain liveness');
check(html.includes("versions=[...new Set("),'fleet warning must show version skew');

// Inference activity provenance: process-local observations may be useful,
// but they are never transaction receipts and divergent hosts must not be
// unioned into an implied canonical feed.
check(html.includes('Selected-Host Inference Activity'),'inference feed must disclose selected-host scope');
check(html.includes('This is not a merged canonical-chain feed.'),'feed copy must reject cross-fork aggregation');
check(!html.includes('Live Inference Feed'),'unqualified live-chain inference label is forbidden');
const normalizerStart=html.indexOf('function normalizeInferenceActivity(');
const detailStart=html.indexOf('function showInferenceDetail(',normalizerStart);
const counterStart=html.indexOf('// These counters are process-local',detailStart);
check(normalizerStart>=0&&detailStart>normalizerStart&&counterStart>detailStart,'inference provenance normalizer and modal must exist');
const normalizerSource=html.slice(normalizerStart,detailStart);
const detailSource=html.slice(detailStart,counterStart);
check(normalizerSource.includes("candidate.schema==='arc.inference.activity.v1'"),'only the explicit v1 schema may carry receipt evidence');
check(normalizerSource.includes("candidate.record_kind==='mined_inference_attestation'"),'mined rows must carry the mined-attestation kind');
check(normalizerSource.includes("candidate.source==='chain_receipt'"),'mined rows must be receipt sourced');
check(normalizerSource.includes('candidate.mined===true'),'mined rows must opt in explicitly');
check(normalizerSource.includes("receiptStatus==='success'||receiptStatus==='failed'"),'receipt status must be explicit');
check(normalizerSource.includes("'legacy.inference.unproven'"),'legacy rows must fail closed as unproven');
check(normalizerSource.includes("record_kind:'inference_observation'"),'unproven rows must become observations');
check(normalizerSource.includes("mined:false")&&normalizerSource.includes("receipt_status:'absent'"),'observations must be explicitly unmined with no receipt');
check(normalizerSource.includes('delete observation.success'),'legacy success=true must be stripped from observations');
check(detailSource.includes("?'Mined Inference Attestation'")&&detailSource.includes(":'Local Inference Observation'"),'modal title must follow provenance');
check(detailSource.includes("['Type',String(a.tx_type||'InferenceObservation')]"),'modal type must come from normalized row provenance');
check(!detailSource.includes("['Type','InferenceAttestation']"),'modal must not hard-code every row as an attestation');
check(detailSource.includes("const txLink=mined&&/^[0-9a-f]{64}$/i.test(hashBare)"),'only mined rows may link to transaction details');
check(detailSource.includes("inf.display_content_source||a.source"),'modal must disclose where displayed inference content came from');
check(detailSource.includes("inf.display_text_on_chain===true?'yes':'no'"),'modal must not imply prompt/output text is stored on-chain');

const inferenceRefreshStart=html.indexOf('// Inference provenance is deliberately pinned to the selected RPC host.');
const recentTransactionsStart=html.indexOf('// Recent transactions - show live stats',inferenceRefreshStart);
check(inferenceRefreshStart>=0&&recentTransactionsStart>inferenceRefreshStart,'selected-host inference refresh section must exist');
const inferenceRefreshSource=html.slice(inferenceRefreshStart,recentTransactionsStart);
check(inferenceRefreshSource.includes('fetch(`${RPC}/inference/attestations?limit=50`'),'activity must come from the selected RPC host');
check(!inferenceRefreshSource.includes('Promise.allSettled(NODES.map'),'inference activity must not union divergent hosts');
check(!inferenceRefreshSource.includes('byHash')&&!inferenceRefreshSource.includes('seen_on'),'host observations must not be deduplicated into a fake chain feed');
check(inferenceRefreshSource.includes("row?.tx_type==='Inference'")&&inferenceRefreshSource.includes("row?.tx_type==='InferenceAttestation'"),'legacy fallback must allow only plausible inference records');
check(!inferenceRefreshSource.includes("row?.tx_type!=='Other'"),'legacy fallback must not accept every unknown transaction type');
check(inferenceRefreshSource.includes("?'MINED · successful receipt'")&&inferenceRefreshSource.includes(":'OBSERVED · not mined'"),'every activity row must expose receipt provenance');
check(inferenceRefreshSource.includes('source: ${a.source_host_name}'),'every activity row must expose its selected host');

const recentTransactionsEnd=html.indexOf("document.getElementById('lastUpdate')",recentTransactionsStart);
const recentTransactionsSource=html.slice(recentTransactionsStart,recentTransactionsEnd);
check(recentTransactionsSource.includes("tx.schema==='arc.inference.activity.v1'"),'transaction list must require the provenance schema');
check(recentTransactionsSource.includes("tx.source==='chain_receipt'")&&recentTransactionsSource.includes('tx.mined===true'),'transaction list must require a receipt-backed mined row');
check(recentTransactionsSource.includes("tx.receipt_status==='success'")&&recentTransactionsSource.includes('tx.success===true'),'failed or absent receipts must not enter recent transactions');
check(!recentTransactionsSource.includes("tx.tx_type!=='Other'"),'transaction filtering must not rely on the old permissive type check');

// High-risk XSS regression checks. Static buttons may retain fixed inline
// handlers, but no server/localStorage record may be serialized into one.
check(!/onclick=['"][^'"]*(?:showInferenceDetail|showTxDetail|verifyHistoryRun|runInferenceOn)/.test(html),'record-driven inline event handlers are forbidden');
check(!/onclick=['"][^'"]*\$\{/.test(html),'dynamic inline event handlers are forbidden');
check(!html.includes('showInferenceDetail(${JSON.stringify(a)})'),'attestation objects must not be serialized into inline JavaScript');

const modalStart=html.indexOf('function showModal(');
const modalEnd=html.indexOf('function closeModal(',modalStart);
check(modalStart>=0&&modalEnd>modalStart,'showModal implementation must exist');
const modalSource=html.slice(modalStart,modalEnd);
check(modalSource.includes('body.replaceChildren()'),'modal rows must be rebuilt with DOM nodes');
check(!modalSource.includes('.innerHTML'),'modal must not render RPC values through innerHTML');
check(modalSource.includes("anchor.rel='noopener noreferrer'"),'external modal links must isolate the opener');

check(html.includes('verify.addEventListener('),'history verification must use an event listener');
check(html.includes("row.addEventListener('click',open)"),'record rows must use event listeners');
check(html.includes('escapeHtml(prompt)'),'user prompts inserted into rich result markup must be escaped');
check(html.includes("escapeHtml(d.output||'')"),'inference output inserted into rich result markup must be escaped');
check(html.includes("makeElement('span','text-[11px] text-white font-medium',name)"),'worker names must render through textContent');

// Compile every inline script without executing browser/network side effects.
const inlineScripts=[...html.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/gi)].map(match=>match[1]);
check(inlineScripts.length>=1,'expected the dashboard application script');
for(const [index,source] of inlineScripts.entries()){
  try{
    new Function(source);
  }catch(error){
    throw new Error(`dashboard contract failed: inline script ${index+1} has invalid JavaScript: ${error.message}`);
  }
  passed+=1;
}

// Duplicate IDs make live updates target the wrong element and are cheap to
// catch without a browser dependency.
const ids=[...html.matchAll(/\sid="([^"]+)"/g)].map(match=>match[1]);
const duplicateIds=[...new Set(ids.filter((id,index)=>ids.indexOf(id)!==index))];
check(duplicateIds.length===0,`duplicate element ids: ${duplicateIds.join(', ')}`);

console.log(`dashboard contract: ${passed} checks passed`);
