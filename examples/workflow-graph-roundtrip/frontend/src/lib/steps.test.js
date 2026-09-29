import { describe, it, expect } from 'vitest';
import {
  buildSteps,
  presentSteps,
  stepTag,
  groupHeader,
  containerSubkindOf,
  stepLabel,
  describeValue,
  humanizeLiteral,
  humanizeIdent,
  operationCatalogEntry,
  toolIconKey,
  friendlyDiagnostic,
  rowDiagnostics,
  stepSummary,
  summarizeFieldValue,
} from './steps.js';

const nd = (id, data) => ({ id, type: data.kind, data });

describe('stepLabel — plain-language, never leaks jargon', () => {
  const cases = [
    [{ kind: 'call', title: 'Show message', operation: 'show_message' }, 'action', 'Do'],
    [{ kind: 'data', binding: 'count', expression: '0' }, 'save', 'Save'],
    [{ kind: 'computation', binding: 'total', expression: 'a + b' }, 'save', 'Save'],
    [{ kind: 'state_update', target: 'count', expression: '(count + 1)' }, 'set', 'Set'],
    [{ kind: 'terminal', terminalKind: 'finish', expression: 'x' }, 'finish', 'Finish with'],
    [{ kind: 'terminal', terminalKind: 'fail', expression: '"e"' }, 'fail', 'Stop with an error'],
    [{ kind: 'process', name: 'onboarding' }, 'process', 'Background task'],
    [{ kind: 'opaque', source: 'foo()' }, 'opaque', 'Advanced step'],
    [{ kind: 'effect', effect: 'start_process', fields: { name: { kind: 'string', value: 'w' } } }, 'start', 'Start'],
    [{ kind: 'effect', effect: 'await_join', fields: { name: { kind: 'string', value: 'w' } } }, 'await', 'Wait for'],
    [{ kind: 'effect', effect: 'wait_signal', fields: { signal: { kind: 'string', value: 'go' } } }, 'waitSignal', 'Wait for'],
    [{ kind: 'effect', effect: 'sleep', fields: { duration: { kind: 'string', value: '400ms' } } }, 'sleep', 'Wait for'],
  ];
  for (const [data, category, lead] of cases) {
    it(`${data.kind}/${data.effect ?? ''} → ${category}`, () => {
      const label = stepLabel(nd('n', data));
      expect(label.category).toBe(category);
      expect(label.lead).toBe(lead);
      // No internal vocabulary escapes into the human lead-in.
      const lc = ` ${label.lead.toLowerCase()} `;
      for (const banned of ['expression', 'state_update', 'comprehension', 'ssa', ' await ', 'terminal']) {
        expect(lc).not.toContain(banned);
      }
    });
  }

  it('maps container sub-kinds to human lead-ins', () => {
    const mk = (subkind) => nd('c', { kind: 'container', subkind, children: [] });
    expect(stepLabel(mk('if'))).toMatchObject({ category: 'if', lead: 'Only if' });
    expect(stepLabel(mk('while'))).toMatchObject({ category: 'while', lead: 'Keep going while' });
    expect(stepLabel(mk('for'))).toMatchObject({ category: 'for', lead: 'For each' });
    expect(stepLabel(mk('comprehension'))).toMatchObject({ category: 'comprehension', lead: 'Make a list' });
  });

  it('uses the for-binding as the each-item name', () => {
    const label = stepLabel(nd('c', { kind: 'container', subkind: 'for', binding: 'row', children: [] }));
    expect(label.name).toBe('row');
  });

  it('uses the operation catalog label and disambiguates duplicate operation names by fields', () => {
    const catalog = [
      {
        id: 'slack.recent',
        operation: 'recent',
        label: 'Get recent Slack messages',
        fields: [{ name: 'channel' }, { name: 'since' }],
      },
      {
        id: 'github.recent',
        operation: 'recent',
        label: 'Get recent GitHub activity',
        fields: [{ name: 'repo' }, { name: 'since' }],
      },
    ];
    const slack = nd('slack', {
      kind: 'call',
      title: 'recent',
      nameSource: 'derived',
      operation: 'recent',
      fields: { channel: { kind: 'string', value: 'team-platform' }, since: { kind: 'string', value: 'yesterday' } },
    });
    const github = nd('github', {
      kind: 'call',
      title: 'recent',
      nameSource: 'derived',
      operation: 'recent',
      fields: { repo: { kind: 'string', value: 'acme/widgets' }, since: { kind: 'string', value: 'yesterday' } },
    });

    expect(operationCatalogEntry(slack, catalog)?.id).toBe('slack.recent');
    expect(stepLabel(slack, catalog)).toMatchObject({
      name: 'Get recent Slack messages',
      icon: 'slack',
    });
    expect(stepLabel(github, catalog)).toMatchObject({
      name: 'Get recent GitHub activity',
      icon: 'github',
    });
  });

  it('preserves an explicitly authored title and humanizes an unknown operation', () => {
    const authored = nd('custom', {
      kind: 'call',
      title: 'Gather the team updates',
      nameSource: 'label',
      operation: 'recent',
    });
    const unknown = nd('unknown', {
      kind: 'call',
      title: 'last_segment',
      nameSource: 'derived',
      operation: 'custom.fetch_records',
    });
    expect(stepLabel(authored, []).name).toBe('Gather the team updates');
    expect(stepLabel(unknown, []).name).toBe('Custom fetch records');
  });
});

describe('toolIconKey — operation identity', () => {
  it.each([
    ['slack.recent', 'slack'],
    ['github.recent', 'github'],
    ['gmail.list_recent', 'email'],
    ['web.search', 'web'],
    ['llm.query', 'llm'],
    ['agents.spawn', 'agent'],
    ['show_message', 'message'],
    ['custom.do_thing', 'action'],
  ])('maps %s to the %s icon', (operation, icon) => {
    expect(toolIconKey(operation)).toBe(icon);
  });
});

describe('describeValue — token vs typed-in (item 3)', () => {
  const vars = [
    { name: 'approval', type: 'any' },
    { name: 'count', type: 'int' },
    { name: 'state.count', type: 'int' },
  ];

  it('renders a value from an earlier step as a typed TOKEN', () => {
    expect(describeValue('approval', vars)).toEqual({ kind: 'token', name: 'approval', varType: 'any' });
    expect(describeValue('count', vars)).toEqual({ kind: 'token', name: 'count', varType: 'int' });
    expect(describeValue('state.count', vars)).toEqual({
      kind: 'token',
      name: 'state.count',
      varType: 'int',
    });
  });

  it('renders a typed-in literal as STATIC text (humanized)', () => {
    expect(describeValue('42', vars)).toMatchObject({ kind: 'static', literalType: 'number', display: '42' });
    expect(describeValue('"Welcome"', vars)).toMatchObject({ kind: 'static', literalType: 'string', display: 'Welcome' });
    expect(describeValue('true', vars)).toMatchObject({ kind: 'static', literalType: 'boolean', display: 'Yes' });
  });

  it('renders a compound value as an EXPRESSION pill', () => {
    expect(describeValue('(count + 1)', vars)).toEqual({ kind: 'expression', text: '(count + 1)' });
    expect(describeValue('compute(x)', vars)).toEqual({ kind: 'expression', text: 'compute(x)' });
  });

  it('a name that is NOT in scope is not a token', () => {
    expect(describeValue('missing', vars).kind).toBe('expression');
  });

  it('an empty value is static empty (renders as a placeholder)', () => {
    expect(describeValue('', vars)).toMatchObject({ kind: 'static', display: '' });
  });
});

describe('humanizers', () => {
  it('humanizeLiteral softens scalars', () => {
    expect(humanizeLiteral({ type: 'string', value: '' })).toBe('(empty)');
    expect(humanizeLiteral({ type: 'boolean', value: 'false' })).toBe('No');
    expect(humanizeLiteral({ type: 'number', value: '7' })).toBe('7');
  });
  it('humanizeIdent turns an operation id into words', () => {
    expect(humanizeIdent('show_message')).toBe('Show message');
    expect(humanizeIdent('set_progress')).toBe('Set progress');
  });
});

describe('stepSummary — collapsed-card gist (item 3)', () => {
  it('joins the first set fields of an action into one calm line', () => {
    const slack = nd('slack', {
      kind: 'call',
      operation: 'recent',
      fields: { channel: { kind: 'string', value: 'team-platform' }, since: { kind: 'string', value: 'yesterday' } },
    });
    expect(stepSummary(slack)).toBe('team-platform · yesterday');
  });

  it('humanizes booleans and numbers and shows an expression arg as its text', () => {
    const node = nd('n', {
      kind: 'call',
      operation: 'do',
      fields: { unread: { kind: 'bool', value: true }, limit: { kind: 'number', value: 20 }, query: { kind: 'expr', value: 'topic' } },
    });
    expect(stepSummary(node)).toBe('Yes · 20 · topic');
  });

  it('skips empty fields and caps at three parts', () => {
    const node = nd('n', {
      kind: 'call',
      operation: 'do',
      fields: { a: { kind: 'string', value: '' }, b: { kind: 'string', value: 'one' }, c: { kind: 'string', value: 'two' }, d: { kind: 'string', value: 'three' }, e: { kind: 'string', value: 'four' } },
    });
    expect(stepSummary(node)).toBe('one · two · three');
  });

  it('is empty for non-action/effect kinds', () => {
    expect(stepSummary(nd('n', { kind: 'data', binding: 'x', expression: '1' }))).toBe('');
    expect(stepSummary(nd('n', { kind: 'call', operation: 'do' }))).toBe('');
    expect(stepSummary(null)).toBe('');
  });

  it('summarizeFieldValue handles each scalar/expr shape', () => {
    expect(summarizeFieldValue({ kind: 'string', value: '  hi  ' })).toBe('hi');
    expect(summarizeFieldValue({ kind: 'bool', value: false })).toBe('No');
    expect(summarizeFieldValue({ kind: 'number', value: 7 })).toBe('7');
    expect(summarizeFieldValue({ kind: 'expr', value: ' count ' })).toBe('count');
    expect(summarizeFieldValue({ kind: 'null', value: null })).toBe('');
  });
});

describe('buildSteps — nesting builder (item 1)', () => {
  const doc = {
    facetSchemaVersion: 1,
    nodes: [
      nd('p', { kind: 'process', name: 'flow', children: [{ slot: 'body', nodeIds: ['c1', 'if1'] }] }),
      nd('c1', { kind: 'call', title: 'Show', operation: 'show_message', fields: { text: { kind: 'string', value: 'hi' } } }),
      nd('if1', {
        kind: 'container',
        subkind: 'if',
        condition: 'true',
        children: [
          { slot: 'then', nodeIds: ['t1'] },
          { slot: 'else', nodeIds: ['e1'] },
        ],
      }),
      nd('t1', { kind: 'data', binding: 'x', expression: '1' }),
      nd('e1', { kind: 'terminal', terminalKind: 'fail', expression: '"bad"' }),
    ],
    roots: { processes: ['p'], main: [] },
  };

  it('walks roots + child groups in source order', () => {
    const tree = buildSteps(doc);
    expect(tree.processes).toHaveLength(1);
    expect(tree.main).toHaveLength(0);
    const proc = tree.processes[0];
    expect(proc.kind).toBe('process');
    expect(proc.groups[0].slot).toBe('body');
    expect(proc.groups[0].steps.map((s) => s.id)).toEqual(['c1', 'if1']);
  });

  it('recurses into if/else with the right headers', () => {
    const tree = buildSteps(doc);
    const ifEntry = tree.processes[0].groups[0].steps[1];
    expect(ifEntry.subkind).toBe('if');
    const [thenG, elseG] = ifEntry.groups;
    expect(thenG.slot).toBe('then');
    expect(thenG.header).toBeNull();
    expect(thenG.steps.map((s) => s.id)).toEqual(['t1']);
    expect(elseG.slot).toBe('else');
    expect(elseG.header).toBe('Otherwise');
    expect(elseG.steps.map((s) => s.id)).toEqual(['e1']);
  });

  it('tracks depth as it descends', () => {
    const tree = buildSteps(doc);
    expect(tree.processes[0].depth).toBe(0);
    expect(tree.processes[0].groups[0].steps[0].depth).toBe(1);
    expect(tree.processes[0].groups[0].steps[1].groups[0].steps[0].depth).toBe(2);
  });

  it('threads typed incoming data dependencies into the consuming step', () => {
    const flow = buildSteps({
      nodes: [
        nd('messages', { kind: 'call', binding: 'messages' }),
        nd('activity', { kind: 'call', binding: 'activity' }),
        nd('agent', {
          kind: 'call',
          operation: 'spawn',
          availableVars: [
            { name: 'messages', type: 'list[{ text: str }]' },
            { name: 'activity', type: 'list[{ title: str }]' },
          ],
        }),
      ],
      edges: [
        { source: 'messages', target: 'agent', data: { kind: 'data', variable: 'messages' } },
        { source: 'activity', target: 'agent', data: { kind: 'data', variable: 'activity' } },
      ],
      roots: { processes: [], main: ['messages', 'activity', 'agent'] },
    });
    expect(flow.main[2].inputs).toEqual([
      { name: 'messages', type: 'list[{ text: str }]' },
      { name: 'activity', type: 'list[{ title: str }]' },
    ]);
  });

  it('is defensive against an empty / missing document', () => {
    expect(buildSteps(null)).toEqual({ processes: [], main: [] });
    expect(buildSteps({ roots: { main: [] } })).toEqual({ processes: [], main: [] });
  });
});

describe('presentSteps — single-process flattening', () => {
  const singleProcess = {
    nodes: [
      nd('p', {
        kind: 'process',
        name: 'team_standup_digest',
        title: 'Team standup digest',
        children: [{ slot: 'body', nodeIds: ['a', 'b', 'fin'] }],
      }),
      nd('a', { kind: 'call', operation: 'slack.recent', fields: { channel: { kind: 'string', value: 'x' } } }),
      nd('b', { kind: 'call', operation: 'github.recent', fields: { repo: { kind: 'string', value: 'y' } } }),
      nd('fin', { kind: 'terminal', terminalKind: 'finish', expression: 'b' }),
    ],
    roots: { processes: ['p'], main: [] },
  };

  it('flattens one process + empty main into the primary flow (no wrapper)', () => {
    const view = presentSteps(singleProcess);
    expect(view.flat).toBe(true);
    expect(view.flowName).toBe('Team standup digest');
    expect(view.steps.map((s) => s.id)).toEqual(['a', 'b', 'fin']);
    expect(view.insertTarget).toEqual({ ownerId: 'p', slot: 'body' });
    expect(view.processes).toEqual([]);
    expect(view.main).toEqual([]);
  });

  it('falls back to the process name when it has no @label title', () => {
    const doc = {
      nodes: [nd('p', { kind: 'process', name: 'raw_name', children: [{ slot: 'body', nodeIds: [] }] })],
      roots: { processes: ['p'], main: [] },
    };
    expect(presentSteps(doc).flowName).toBe('raw_name');
  });

  it('keeps the grouped presentation when there are multiple processes', () => {
    const doc = {
      nodes: [
        nd('p1', { kind: 'process', name: 'a', children: [{ slot: 'body', nodeIds: [] }] }),
        nd('p2', { kind: 'process', name: 'b', children: [{ slot: 'body', nodeIds: [] }] }),
      ],
      roots: { processes: ['p1', 'p2'], main: [] },
    };
    const view = presentSteps(doc);
    expect(view.flat).toBe(false);
    expect(view.processes).toHaveLength(2);
    expect(view.insertTarget).toBeNull();
  });

  it('keeps the grouped presentation when a top-level main coexists with a process', () => {
    const doc = {
      nodes: [
        nd('p', { kind: 'process', name: 'a', children: [{ slot: 'body', nodeIds: [] }] }),
        nd('m', { kind: 'call', operation: 'x' }),
      ],
      roots: { processes: ['p'], main: ['m'] },
    };
    const view = presentSteps(doc);
    expect(view.flat).toBe(false);
    expect(view.main).toHaveLength(1);
    expect(view.processes).toHaveLength(1);
  });

  it('a plain top-level (main only, no process) is not flattened', () => {
    const doc = {
      nodes: [nd('m', { kind: 'call', operation: 'x' })],
      roots: { processes: [], main: ['m'] },
    };
    const view = presentSteps(doc);
    expect(view.flat).toBe(false);
    expect(view.main.map((s) => s.id)).toEqual(['m']);
  });
});

describe('stepTag — trigger vs action vs step', () => {
  it('the first step of a primary flow is the Trigger', () => {
    expect(stepTag(true, 0)).toEqual({ role: 'trigger', text: 'Trigger' });
  });
  it('later primary steps are numbered Actions', () => {
    expect(stepTag(true, 1)).toEqual({ role: 'action', text: 'Action 2' });
    expect(stepTag(true, 4)).toEqual({ role: 'action', text: 'Action 5' });
  });
  it('non-primary (branch/loop) steps are plain numbered Steps', () => {
    expect(stepTag(false, 0)).toEqual({ role: 'step', text: 'Step 1' });
    expect(stepTag(false, 2)).toEqual({ role: 'step', text: 'Step 3' });
  });
});

describe('groupHeader / containerSubkindOf', () => {
  it('only the if-else arm gets a header', () => {
    expect(groupHeader('if', 'then')).toBeNull();
    expect(groupHeader('if', 'else')).toBe('Otherwise');
    expect(groupHeader('for', 'body')).toBeNull();
    expect(groupHeader('process', 'body')).toBeNull();
  });
  it('recovers a sub-kind from slots when subkind is absent', () => {
    expect(containerSubkindOf({ data: { children: [{ slot: 'then' }, { slot: 'else' }] } })).toBe('if');
    expect(containerSubkindOf({ data: { children: [{ slot: 'element' }] } })).toBe('comprehension');
    expect(containerSubkindOf({ data: { subkind: 'while', children: [] } })).toBe('while');
  });
});

describe('friendlyDiagnostic / rowDiagnostics (item 4)', () => {
  it('softens known type-error kinds', () => {
    expect(friendlyDiagnostic({ kind: 'incompatible_iteration_target', message: 'x' })).toMatch(/not a list/);
    expect(friendlyDiagnostic({ kind: 'incompatible_process_return', message: 'x' })).toMatch(/background task/);
  });
  it('falls back to the backend message for unknown kinds', () => {
    expect(friendlyDiagnostic({ kind: 'weird', message: 'raw detail' })).toBe('raw detail');
  });
  it('maps a node’s diagnostics to friendly row sentences', () => {
    const node = nd('n', {
      kind: 'container',
      diagnostics: [{ nodeId: 'n', kind: 'incompatible_iteration_target', message: 'technical' }],
    });
    const rows = rowDiagnostics(node);
    expect(rows).toHaveLength(1);
    expect(rows[0].friendly).toMatch(/not a list/);
    expect(rows[0].message).toBe('technical');
  });
  it('a clean node has no diagnostic rows', () => {
    expect(rowDiagnostics(nd('n', { kind: 'call' }))).toEqual([]);
  });
});
