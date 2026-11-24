import assert from 'node:assert/strict';
import path from 'node:path';

const dir = process.env.CCP_TEST_GARCON + '/server-agents/codex/src/agents/codex/app-server';
const { CodexAppServerClient } = await import(dir + '/client.ts');
const { CodexAppServerRuntime } = await import(dir + '/runtime.ts');
const { buildThreadStartParams, buildThreadResumeParams } = await import(dir + '/request-builders.ts');
const [a, b] = process.argv.slice(2);
const settings = { projectPath: process.cwd(), model: 'gpt-6.2-sol', permissionMode: 'default' };
const resume = (c, id, nativePath) =>
  c.resumeThread(buildThreadResumeParams({ ...settings, agentSessionId: id, nativePath }));
const warnings = [];
const makeClient = (options) => new CodexAppServerClient(options).on('warning', m => warnings.push(m));
let publicInit;
async function connection(run) {
  const c = makeClient();
  try {
    const init = await c.connect();
    if (!publicInit) publicInit = init;
    assert.deepEqual(init, publicInit);
    assert.match(init.userAgent, /^garcon\/0\.172\.0\b/);
    assert(![a, b, process.env.CODEX_HOME].includes(init.codexHome));
    return await run(c);
  } finally { await c.shutdown(); }
}

await connection(() => {}); // An initialize-only connection must not consume the first route.
const parent = await connection(async c => {
  const { thread } = await c.startThread(buildThreadStartParams(settings));
  assert.equal(thread.id, thread.sessionId);
  assert.equal(thread.ephemeral, false);
  assert(thread.path.startsWith(a + '/sessions/'));
  assert.deepEqual((await c.loadedThreads()).data, [thread.id]);
  assert.equal((await c.request('config/read', {})).config.model_reasoning_effort, 'low');
  return thread;
});
// 0.172.0 assigns the path before a no-turn start writes a rollout.
assert.equal(await Bun.file(parent.path).exists(), false);
await connection(c => assert.rejects(resume(c, parent.id, parent.path), /no rollout found/));

async function seed(id, file) {
  const timestamp = new Date().toISOString();
  const meta = { id, timestamp, cwd: settings.projectPath, originator: 'codex_cli_rs',
                 cli_version: '0.172.0', source: 'cli', model_provider: 'openai' };
  const item = { type: 'message', role: 'user', content: [{ type: 'input_text', text: 'offline fixture' }] };
  const turn = {type:'user_message', message:'offline fixture', images:[], local_images:[], text_elements:[]};
  await Bun.write(file, [{type:'session_meta', payload:meta, timestamp},
                         {type:'response_item', payload:item, timestamp},
                         {type:'event_msg', payload:turn, timestamp}].map(r => JSON.stringify(r)).join('\n') + '\n');
}
await seed(parent.id, parent.path);
const other = crypto.randomUUID();
const decoy = path.join(b, path.relative(a, parent.path).replace(parent.id, other));
await seed(other, decoy);

await connection(async c => {
  const list = {limit:1, sortKey:'updated_at', sortDirection:'desc', archived:false, useStateDbOnly:false};
  const page = await c.listThreads(list);
  assert(page.nextCursor, JSON.stringify(page.data.map(t=>[t.id,t.path])));
  const last = await c.listThreads({...list, cursor:page.nextCursor});
  assert.equal(last.nextCursor, null);
  assert.deepEqual(new Set([...page.data,...last.data].map(t => t.id)), new Set([parent.id,other]));
  // Native gives path precedence. Deliberately supply a real different rollout.
  const cold = await resume(c, parent.id, decoy);
  assert.equal(cold.thread.id, parent.id);
  assert.equal(cold.thread.path, parent.path);
  assert.equal((await resume(c, parent.id, decoy)).thread.id, parent.id);
  assert(!(await c.listThreads({...list,limit:500})).data.some(t => t.id === other));
  await assert.rejects(resume(c, other, parent.path), /open a new connection|not available on this connection/i);
});

// Runtime forkSession uses its own cold client, unsubscribes the child,
// and waits for the native rollout to materialize before returning.
const runtime = new CodexAppServerRuntime({ createClient: makeClient });
let child;
try {
  child = await runtime.forkSession({ sourceSession: {
    ...settings, agentSessionId: parent.id, nativePath: decoy,
  } });
} finally { await runtime.shutdown(); }
assert.notEqual(child.agentSessionId, parent.id);
assert(child.nativePath.startsWith(a + '/sessions/'));
await connection(async c => {
  const { thread } = await resume(c, child.agentSessionId, decoy);
  assert.equal(thread.id, child.agentSessionId);
  assert.equal(thread.path, child.nativePath);
  assert.equal(thread.sessionId, thread.id);
  assert.equal(thread.ephemeral, false);
  assert.equal(thread.parentThreadId, null);
  assert.equal(thread.forkedFromId, parent.id);
});
const next = await connection(async c => {
  const result = await c.startThread(buildThreadStartParams(settings));
  assert.equal((await resume(c, other, parent.path)).thread.path, decoy);
  return result;
});
assert(next.thread.path.startsWith(b + '/sessions/'), 'cold restores/fork advanced the cursor');
// Garcon reports unsolicited/duplicate initialize results as late responses.
assert.deepEqual(warnings, []);
console.log(JSON.stringify({ parent: parent.id, child: child.agentSessionId, next: next.thread.id }));
