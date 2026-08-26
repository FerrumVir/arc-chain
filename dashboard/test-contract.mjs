#!/usr/bin/env node

import {readFileSync} from 'node:fs';

const dashboardUrl=new URL('./index.html',import.meta.url);
const html=readFileSync(dashboardUrl,'utf8');
let passed=0;

function check(condition,message){
  if(!condition){
    throw new Error(`dashboard contract failed: ${message}`);
  }
  passed+=1;
}

check(html.startsWith('<!DOCTYPE html>'),'index.html must remain a standalone HTML document');

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
check(html.includes('const work=completedJobsOf(w);'),'worker cards must use the same completed-job accessor');
check(!/const work\s*=\s*w\.work_completed\s*\|\|/.test(html),'worker cards must not ignore success_count');

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
check(inlineScripts.length>=2,'expected Tailwind config and dashboard application scripts');
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
