import assert from 'node:assert/strict';

const src = process.env.FIXTURE_SRC;
const load = (name) => import(src + '/agents/claude/' + name + '.ts');
const { createClaudeConfig } = await import(src + '/config.ts');
const { ClaudeCliRuntime } = await load('claude-cli');
const { ClaudeCliVersionProbe } = await load('cli-version');
const { buildClaudeCLIArgs, runSingleQuery } = await load('cli-invocation');
const { getClaudeAuthStatus } = await load('claude-auth');
const { ClaudeSlashCommandDiscovery } = await load('slash-command-discovery');
const config = createClaudeConfig({ get: (k) => process.env[k] });
const logger = { debug() {}, info() {}, warn() {}, error() {} };
const deps = { logger, binary: config.binary, versionProbe: new ClaudeCliVersionProbe() };
const id = crypto.randomUUID();
const completed = [];
const turn = (command) => ({
  agentSessionId: id, chatId: 'test', projectPath: process.cwd(), model: 'sonnet[200k]',
  command, permissionMode: 'default', thinkingMode: 'high',
  operation: { runId: command, publish(e) {
    if (e.type === 'run-ended') completed.push([command, e.outcome, e.finalResponse?.text]);
  } },
});

assert.equal(config.binary(), 'claude');
assert.equal((await getClaudeAuthStatus(config)).label, 'other');
let runtime = new ClaudeCliRuntime(deps);
try {
  await runtime.startClaudeCliSession(turn('fresh'));
  await runtime.runClaudeTurn(turn('warm'));
} finally { await runtime.shutdown(); }
// A one-shot uses the other account and must not replace the original binding.
process.env.CLAUDE_BINARY = process.env.FIXTURE_ALIAS;
assert.equal((await runSingleQuery('one-shot', { model: 'sonnet[200k]' }, deps)).trim(), 'other');
runtime = new ClaudeCliRuntime(deps);
try {
  await runtime.runClaudeTurn(turn('cold'));
} finally { await runtime.shutdown(); }
assert.deepEqual(completed, ['fresh', 'warm', 'cold'].map(command => [command, 'finished', 'first']));
const calls = (await Bun.file('first/calls').text()).trim().split('\n').map(s => JSON.parse(s));
assert.deepEqual(calls, [{ sessionId: id }, { resumeSessionId: id }].map(selector =>
  buildClaudeCLIArgs({ ...turn(''), streamJson: true, ...selector })));
const discovery = new ClaudeSlashCommandDiscovery(config.binary, () => undefined, logger);
assert.deepEqual((await discovery.discover(process.cwd())).map(c => c.name), ['fixture']);
await assert.rejects(runSingleQuery('not run', {
  envOverrides: { ANTHROPIC_BASE_URL: 'https://fixture.invalid', ANTHROPIC_AUTH_TOKEN: 'fixture-only' },
}, deps), /exited with code/);
console.log(JSON.stringify(id));
