pub(crate) const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Tic Tac Toe Agent</title>
  <style>
    @import url('https://fonts.googleapis.com/css2?family=Big+Shoulders+Display:wght@600;760&family=Chivo+Mono:wght@400;600&family=Spectral:wght@400;600&display=swap');
    :root {
      color-scheme: dark;
      --ink: oklch(0.91 0.018 78);
      --muted: oklch(0.68 0.025 78);
      --dim: oklch(0.48 0.023 78);
      --line: oklch(0.31 0.027 78);
      --paper: oklch(0.15 0.018 78);
      --panel: oklch(0.19 0.019 78);
      --panel-2: oklch(0.23 0.021 78);
      --sun: oklch(0.74 0.155 65);
      --human: oklch(0.73 0.112 165);
      --agent: oklch(0.68 0.12 48);
      --bad: oklch(0.62 0.19 32);
      font-family: Spectral, Georgia, serif;
    }
    * { box-sizing: border-box; }
    html { width:100%; height:100%; min-height:100%; overflow:hidden; background:var(--paper); }
    body { width:100%; height:100%; min-height:100%; overflow:hidden; margin:0; background:var(--paper); color:var(--ink); }
    body::before { content:""; position:fixed; inset:0; pointer-events:none; opacity:.22; background:repeating-linear-gradient(135deg, transparent 0 10px, oklch(0.24 0.025 78) 10px 11px); }
    .app { display:grid; grid-template-columns:312px minmax(0, 1fr); width:100%; height:100vh; height:100dvh; min-height:100vh; min-height:100dvh; overflow:hidden; position:relative; background:var(--panel); }
    aside { border-right:1px solid var(--line); background:oklch(0.17 0.019 78); display:flex; flex-direction:column; min-width:0; }
    header { padding:20px; border-bottom:1px solid var(--line); display:grid; gap:12px; }
    h1 { margin:0; font-family:"Big Shoulders Display", sans-serif; font-size:32px; line-height:.92; letter-spacing:.01em; text-transform:uppercase; }
    .subhead { color:var(--muted); font-size:13px; line-height:1.35; }
    button, select, input, textarea { font:inherit; }
    button { border:1px solid var(--sun); background:var(--sun); color:oklch(0.15 0.018 78); border-radius:3px; padding:9px 12px; cursor:pointer; font-family:"Chivo Mono", monospace; font-size:13px; font-weight:600; }
    button.secondary { background:transparent; color:var(--ink); border-color:var(--line); }
    button:disabled { opacity:.55; cursor:default; }
    .chat-list { overflow:auto; padding:12px; display:grid; gap:8px; }
    .chat-row { width:100%; text-align:left; background:transparent; color:var(--ink); border-color:transparent; display:grid; gap:3px; padding:10px; }
    .chat-row.active { border-color:var(--sun); background:var(--panel); }
    .chat-title, .chat-model { overflow:hidden; white-space:nowrap; text-overflow:ellipsis; }
    .chat-model { color:var(--muted); font:12px "Chivo Mono", monospace; }
    main { min-width:0; min-height:0; display:grid; grid-template-rows:auto auto auto minmax(0,1fr) auto; background:var(--panel); overflow:hidden; }
    .topbar { min-height:66px; padding:12px 20px; border-bottom:1px solid var(--line); display:flex; justify-content:space-between; gap:16px; align-items:center; background:oklch(0.18 0.019 78); }
    .topbar-title { display:grid; gap:2px; min-width:0; }
    .topbar-title strong { font-family:"Chivo Mono", monospace; font-size:13px; color:var(--sun); }
    .topbar-title span { color:var(--muted); font-size:12px; }
    .model-controls { display:grid; grid-template-columns:minmax(210px, 340px) 120px; gap:8px; align-items:end; margin-left:auto; }
    .model-controls .field { gap:4px; }
    .model-controls input, .model-controls select { font-family:"Chivo Mono", monospace; font-size:12px; padding:8px; }
    .branchbar { min-height:52px; padding:9px 20px; border-bottom:1px solid var(--line); display:flex; align-items:center; gap:8px; background:oklch(0.17 0.02 78); }
    .branchbar select { width:min(420px, 40vw); font:12px "Chivo Mono", monospace; padding:8px; }
    .branchbar .status { margin-left:auto; color:var(--muted); font:12px "Chivo Mono", monospace; text-align:right; }
    .game { border-bottom:1px solid var(--line); padding:22px 26px; display:grid; grid-template-columns:auto minmax(260px,380px); grid-template-areas:"board status" "board actions"; gap:14px 26px; align-items:start; justify-content:start; background:var(--panel-2); }
    .board { display:grid; grid-template-columns:repeat(3,64px); grid-template-rows:repeat(3,64px); gap:7px; }
    .game .board { grid-area:board; }
    .cell { width:64px; height:64px; display:grid; place-items:center; border:1px solid var(--line); border-radius:5px; background:oklch(0.16 0.018 78); color:var(--ink); font:34px/1 "Big Shoulders Display", sans-serif; padding:0; }
    .cell:not(:disabled):hover { border-color:var(--sun); background:oklch(0.21 0.025 78); }
    .cell.x { color:var(--human); }
    .cell.o { color:var(--agent); }
    .cell.win { border-color:var(--sun); background:oklch(0.22 0.045 92); box-shadow:inset 0 0 0 1px color-mix(in oklch, var(--sun), transparent 45%); }
    .cell:disabled { opacity:1; cursor:default; }
    .game-status { grid-area:status; display:grid; gap:7px; min-width:0; align-self:end; }
    .game-status strong { font:24px/1 "Big Shoulders Display", sans-serif; letter-spacing:0; text-transform:uppercase; }
    .game-status span { color:var(--muted); font-size:13px; }
    .game-status.done strong { color:var(--sun); }
    .game-status.done span { color:var(--ink); }
    #resetBoard { grid-area:actions; justify-self:start; align-self:start; min-height:42px; }
    .players { display:flex; gap:8px; flex-wrap:wrap; }
    .player { border:1px solid var(--line); border-radius:5px; padding:8px 10px; background:oklch(0.18 0.018 78); display:grid; gap:2px; min-width:108px; }
    .player b { font:22px/1 "Big Shoulders Display", sans-serif; }
    .player span { color:var(--muted); font:12px "Chivo Mono", monospace; }
    .player.you { border-color:color-mix(in oklch, var(--human), var(--line)); }
    .player.agent { border-color:color-mix(in oklch, var(--agent), var(--line)); }
    .messages { min-height:0; overflow:auto; padding:20px; display:grid; align-content:start; gap:14px; overscroll-behavior:contain; }
    .msg { max-width:min(820px, 78%); white-space:pre-wrap; line-height:1.45; border:1px solid var(--line); border-radius:5px; padding:10px 12px; background:oklch(0.18 0.018 78); }
    .msg.user { justify-self:end; border-color:color-mix(in oklch, var(--human), var(--line)); background:oklch(0.18 0.024 150); }
    .msg.system { border-color:color-mix(in oklch, var(--bad), var(--line)); color:var(--muted); }
    .meta { color:var(--muted); font:12px "Chivo Mono", monospace; margin-bottom:4px; }
    .tool { max-width:min(820px, 82%); border:1px solid var(--line); border-radius:5px; padding:11px; background:oklch(0.17 0.02 78); font-size:13px; display:grid; gap:8px; }
    .tool-head { display:flex; gap:8px; align-items:center; flex-wrap:wrap; }
    .tool strong { color:var(--sun); font-family:"Chivo Mono", monospace; }
    .tool.fail strong { color:var(--bad); }
    .badge { border:1px solid var(--line); border-radius:999px; padding:2px 7px; color:var(--muted); background:oklch(0.16 0.018 78); font:12px "Chivo Mono", monospace; }
    .tool-summary { color:var(--ink); }
    .reasoning, .code-block { max-width:min(820px, 82%); border:1px dashed var(--line); border-radius:5px; padding:9px 11px; background:oklch(0.16 0.018 78); color:var(--muted); }
    .reasoning { border-color:color-mix(in oklch, var(--sun), var(--line)); }
    .reasoning summary, .code-block summary { cursor:pointer; font-family:"Chivo Mono", monospace; font-size:12px; color:var(--sun); }
    .reasoning pre, .code-block pre { white-space:pre-wrap; margin:8px 0 0; font-size:12px; line-height:1.45; overflow:auto; }
    .code-block.fail summary { color:var(--bad); }
    .tool details { border-top:1px solid var(--line); padding-top:7px; }
    .tool summary { color:var(--muted); cursor:pointer; }
    .tool pre { overflow:auto; margin:8px 0 0; font-size:12px; }
    form { border-top:1px solid var(--line); padding:14px; display:grid; grid-template-columns:1fr auto; gap:10px; align-items:end; background:oklch(0.18 0.019 78); }
    .field { display:grid; gap:5px; min-width:0; }
    label { font-size:12px; color:var(--muted); }
    textarea, input, select { width:100%; border:1px solid var(--line); border-radius:3px; padding:9px; background:oklch(0.14 0.018 78); color:var(--ink); }
    textarea { min-height:76px; resize:vertical; }
    @media (max-width: 760px) {
      .app { grid-template-columns:1fr; }
      aside { display:none; }
      .topbar { padding:11px 14px; display:grid; grid-template-columns:1fr auto; }
      .model-controls { grid-column:1 / -1; grid-template-columns:1fr 118px; width:100%; margin-left:0; }
      .branchbar { padding:10px 14px; flex-wrap:wrap; }
      .branchbar select { width:100%; order:3; }
      .branchbar .status { width:100%; margin-left:0; text-align:left; }
      .game { grid-template-columns:1fr; grid-template-areas:"board" "status" "actions"; padding:18px 14px; justify-items:start; }
      .game-status { align-self:start; }
      form { grid-template-columns:1fr; }
    }
  </style>
</head>
<body>
  <div class="app">
    <aside>
      <header>
        <h1>Tic Tac Toe Agent</h1>
        <div class="subhead">You play X. The RLM agent plays O through app-owned board tools.</div>
        <button id="newChat">New chat</button>
      </header>
      <div id="chats" class="chat-list"></div>
    </aside>
    <main>
      <div class="topbar">
        <div class="topbar-title">
          <strong id="activeTitle">No chat</strong>
          <span>Board state is sent to the agent each turn</span>
        </div>
        <div class="model-controls">
          <label class="field">Model
            <input id="modelInput" autocomplete="off" spellcheck="false" />
          </label>
          <label class="field">Variant
            <select id="variantInput">
              <option value="">default</option>
            </select>
          </label>
        </div>
        <button id="mobileNew" class="secondary">New chat</button>
      </div>
      <div class="branchbar" aria-label="Branch controls">
        <button id="pinBranch" class="secondary" type="button">Pin current turn</button>
        <select id="branchPoint" aria-label="Pinned turn">
          <option value="">No pinned turns</option>
        </select>
        <button id="forkBranch" type="button">Fork from pin</button>
        <span id="branchStatus" class="status">Pin a completed turn to preserve a branch point.</span>
      </div>
      <section class="game">
        <div id="board" class="board"></div>
        <div id="gameStatusBlock" class="game-status">
          <div class="players">
            <div class="player you"><b>X</b><span>You</span></div>
            <div class="player agent"><b>O</b><span>Agent</span></div>
          </div>
          <strong id="gameStatus">X to move</strong>
          <span id="gameHint">Click any empty square. The agent replies automatically as O.</span>
        </div>
        <button id="resetBoard" class="secondary" type="button">Reset board</button>
      </section>
      <div id="messages" class="messages"></div>
      <form id="composer">
        <textarea id="text" placeholder="Optional: add a note for the agent. Board clicks send turns automatically."></textarea>
        <button id="send" type="submit">Send</button>
      </form>
    </main>
  </div>
  <script>
    const chatsEl = document.querySelector('#chats');
    const messagesEl = document.querySelector('#messages');
    const titleEl = document.querySelector('#activeTitle');
    const form = document.querySelector('#composer');
    const modelInput = document.querySelector('#modelInput');
    const variantInput = document.querySelector('#variantInput');
    const boardEl = document.querySelector('#board');
    const gameStatusEl = document.querySelector('#gameStatus');
    const gameStatusBlockEl = document.querySelector('#gameStatusBlock');
    const gameHintEl = document.querySelector('#gameHint');
    const resetBoardBtn = document.querySelector('#resetBoard');
    const pinBranchBtn = document.querySelector('#pinBranch');
    const forkBranchBtn = document.querySelector('#forkBranch');
    const branchPointEl = document.querySelector('#branchPoint');
    const branchStatusEl = document.querySelector('#branchStatus');
    let chats = [];
    let activeChat = null;
    let settings = { default_model:'anthropic/claude-sonnet-4.6', default_model_variant:'high', model_variants:['low','medium','high'] };
    let viewGeneration = 0;
    let messageReadGeneration = 0;
    let branchReadGeneration = 0;
    let activeRun = null;
    let branchPoints = [];
    const boards = new Map();

    function currentView() {
      return { chatId:activeChat, generation:viewGeneration };
    }
    function isCurrentView(owner) {
      return owner.chatId === activeChat && owner.generation === viewGeneration;
    }
    function isBusy() {
      return activeRun !== null;
    }
    async function selectChat(chatId) {
      activeChat = chatId;
      const view = { chatId, generation:++viewGeneration };
      messagesEl.innerHTML = '';
      branchPoints = [];
      renderChats();
      renderBoard();
      renderBranchPoints();
      await Promise.all([loadMessages(chatId, view), loadBranchPoints(chatId, view)]);
      return view;
    }
    function emptyBoard() {
      return { cells:Array(9).fill(null), turn:'X' };
    }
    function currentBoard() {
      if (!activeChat) return emptyBoard();
      if (!boards.has(activeChat)) boards.set(activeChat, emptyBoard());
      return boards.get(activeChat);
    }
    function winner(cells) {
      const lines = [[0,1,2],[3,4,5],[6,7,8],[0,3,6],[1,4,7],[2,5,8],[0,4,8],[2,4,6]];
      for (const [a,b,c] of lines) if (cells[a] && cells[a] === cells[b] && cells[a] === cells[c]) return cells[a];
      return null;
    }
    function winningLine(cells) {
      const lines = [[0,1,2],[3,4,5],[6,7,8],[0,3,6],[1,4,7],[2,5,8],[0,4,8],[2,4,6]];
      return lines.find(([a,b,c]) => cells[a] && cells[a] === cells[b] && cells[a] === cells[c]) || [];
    }
    function boardStatus(board) {
      const win = winner(board.cells);
      if (win === 'X') return 'You won';
      if (win === 'O') return 'Agent won';
      if (board.cells.every(Boolean)) return 'Draw';
      return `${board.turn} to move`;
    }
    function terminalHint(board) {
      const win = winner(board.cells);
      if (win === 'X') return 'You won this round.';
      if (win === 'O') return 'Agent won this round.';
      if (board.cells.every(Boolean)) return 'The round ended in a draw.';
      return null;
    }
    function cellName(index) {
      return ['top left','top middle','top right','middle left','center','middle right','bottom left','bottom middle','bottom right'][index] || `cell ${index}`;
    }
    function renderBoard() {
      const board = currentBoard();
      boardEl.innerHTML = '';
      const done = Boolean(winner(board.cells)) || board.cells.every(Boolean);
      const winCells = new Set(winningLine(board.cells));
      board.cells.forEach((mark, index) => {
        const cell = document.createElement('button');
        cell.type = 'button';
        cell.className = `cell ${mark ? mark.toLowerCase() : ''}${winCells.has(index) ? ' win' : ''}`;
        cell.textContent = mark || '';
        cell.ariaLabel = `cell ${index}`;
        cell.disabled = isBusy() || Boolean(mark) || board.turn !== 'X' || done;
        cell.onclick = () => playHuman(index);
        boardEl.appendChild(cell);
      });
      gameStatusBlockEl.classList.toggle('done', done);
      gameStatusEl.textContent = boardStatus(board);
      resetBoardBtn.disabled = isBusy() || !activeChat;
      pinBranchBtn.disabled = isBusy() || !activeChat;
      forkBranchBtn.disabled = isBusy() || !activeChat || !branchPoints.length;
      gameHintEl.textContent = done
        ? `${terminalHint(board)} Reset the board to start another round.`
        : isBusy()
          ? 'Agent is thinking and may call board tools.'
        : board.turn === 'X'
          ? 'Your turn: click any empty square.'
          : 'Agent turn: waiting for O to play.';
    }
    function selectedModel() {
      return {
        model: modelInput.value.trim() || settings.default_model,
        model_variant: variantInput.value || null
      };
    }
    function setModelControls(chat) {
      const model = chat?.model || settings.default_model;
      const variant = chat?.model_variant ?? settings.default_model_variant ?? '';
      modelInput.value = model;
      variantInput.value = variant || '';
    }
    async function loadSettings() {
      settings = await (await api('/api/settings')).json();
      variantInput.innerHTML = '<option value="">default</option>';
      for (const variant of settings.model_variants || []) {
        const option = document.createElement('option');
        option.value = variant;
        option.textContent = variant;
        variantInput.appendChild(option);
      }
      setModelControls(null);
    }
    async function saveActiveModel() {
      if (!activeChat || isBusy()) return;
      const selection = selectedModel();
      const chat = await (await api(`/api/chats/${activeChat}/model`, {
        method:'POST',
        body: JSON.stringify(selection)
      })).json();
      const index = chats.findIndex(item => item.id === chat.id);
      if (index >= 0) chats[index] = chat;
      renderChats();
    }
    function setBoard(owner, board) {
      if (!owner.chatId || !board || !Array.isArray(board.cells)) return;
      const cells = board.cells.slice(0, 9).map(cell => cell === 'X' || cell === 'O' ? cell : null);
      const terminal = Boolean(winner(cells)) || cells.every(Boolean);
      boards.set(owner.chatId, {
        cells,
        turn: terminal ? 'X' : board.turn === 'O' ? 'O' : 'X'
      });
      if (isCurrentView(owner)) renderBoard();
    }
    async function resetBoard() {
      if (isBusy()) return;
      if (!activeChat) await newChat();
      if (!activeChat) return;
      boards.set(activeChat, emptyBoard());
      renderBoard();
      await sendText('I reset the board.');
    }
    function playHuman(index) {
      const board = currentBoard();
      if (board.turn !== 'X' || board.cells[index] || winner(board.cells)) return;
      board.cells[index] = 'X';
      board.turn = 'O';
      renderBoard();
      sendText(`I played X in the ${cellName(index)}.`);
    }
    function applyToolBoard(event, owner) {
      const raw = toolResult(event);
      const board = raw?.board?.cells ? raw.board : raw;
      if (board?.cells) setBoard(owner, board);
    }
    function terminalToolSummary(board) {
      if (!board?.cells) return null;
      const win = winner(board.cells);
      if (win === 'O') return 'Agent won this round.';
      if (win === 'X') return 'You won this round.';
      if (board.cells.every(Boolean)) return 'The round ended in a draw.';
      return null;
    }
    function cleanArgs(args) {
      const out = { ...(args || {}) };
      delete out.__session_id__;
      return out;
    }
    function toolResult(event) {
      return event?.result
        ?? event?.output?.outcome?.payload
        ?? event?.output?.value
        ?? null;
    }
    function toolSucceeded(event) {
      if (typeof event?.success === 'boolean') return event.success;
      return event?.output?.outcome?.status === 'success';
    }
    function compactToolPayload(event) {
      const raw = toolResult(event);
      if (event.name === 'play_move') {
        return {
          args: cleanArgs(event.args),
          accepted: raw?.accepted,
          move: raw?.move,
          status: raw?.board?.status,
          turn: raw?.board?.turn,
          winner: raw?.board?.winner
        };
      }
      if (event.name === 'read_board') {
        return {
          args: cleanArgs(event.args),
          status: raw?.status,
          turn: raw?.turn,
          legal_moves: raw?.legal_moves,
          winner: raw?.winner,
          marks_by_index: raw?.marks_by_index
        };
      }
      return { args: cleanArgs(event.args), result: raw };
    }
    function renderTerminalValue(value) {
      if (value === null || value === undefined) return '';
      if (typeof value === 'string') return value;
      return JSON.stringify(value, null, 2);
    }

    async function api(url, options = {}) {
      const res = await fetch(url, { headers: { 'content-type': 'application/json' }, ...options });
      if (!res.ok) throw new Error((await res.json()).error || res.statusText);
      return res;
    }
    async function loadChats(view = currentView()) {
      chats = await (await api('/api/chats')).json();
      renderChats();
      if (!activeChat && chats[0]) {
        await selectChat(chats[0].id);
      } else if (activeChat && isCurrentView(view) && !isBusy()) {
        await Promise.all([loadMessages(view.chatId, view), loadBranchPoints(view.chatId, view)]);
      }
    }
    function renderChats() {
      chatsEl.innerHTML = '';
      for (const chat of chats) {
        const b = document.createElement('button');
        b.className = 'chat-row' + (chat.id === activeChat ? ' active' : '');
        b.innerHTML = `<span class="chat-title"></span><span class="chat-model"></span>`;
        b.querySelector('.chat-title').textContent = chat.title;
        b.querySelector('.chat-model').textContent = chat.model_label;
        b.onclick = () => selectChat(chat.id);
        chatsEl.appendChild(b);
      }
      const current = chats.find(c => c.id === activeChat);
      titleEl.textContent = current ? current.title : 'No chat';
      if (current) setModelControls(current);
    }
    async function newChat() {
      const view = currentView();
      const chat = await (await api('/api/chats', { method:'POST', body: JSON.stringify(selectedModel()) })).json();
      chats.unshift(chat);
      boards.set(chat.id, emptyBoard());
      if (isCurrentView(view)) await selectChat(chat.id);
      else renderChats();
    }
    function renderBranchPoints() {
      branchPointEl.innerHTML = '';
      for (const point of branchPoints) {
        const option = document.createElement('option');
        option.value = point.node_id;
        const pinnedAt = new Date(point.created_at).toLocaleString();
        option.textContent = `${point.message_count} messages · ${pinnedAt}`;
        branchPointEl.appendChild(option);
      }
      if (!branchPoints.length) {
        const option = document.createElement('option');
        option.value = '';
        option.textContent = 'No pinned turns';
        branchPointEl.appendChild(option);
      }
      pinBranchBtn.disabled = isBusy() || !activeChat;
      forkBranchBtn.disabled = isBusy() || !activeChat || !branchPoints.length;
      branchPointEl.disabled = isBusy() || !branchPoints.length;
      branchStatusEl.textContent = branchPoints.length
        ? `${branchPoints.length} retained turn${branchPoints.length === 1 ? '' : 's'} available`
        : 'Pin a completed turn to preserve a branch point.';
    }
    async function loadBranchPoints(chatId, view) {
      if (!isCurrentView(view)) return;
      const requestGeneration = ++branchReadGeneration;
      const points = await (await api(`/api/chats/${chatId}/branch-points`)).json();
      if (!isCurrentView(view) || requestGeneration !== branchReadGeneration) return;
      branchPoints = points;
      renderBranchPoints();
    }
    async function pinCurrentTurn() {
      if (!activeChat || isBusy()) return;
      const view = currentView();
      pinBranchBtn.disabled = true;
      branchStatusEl.textContent = 'Pinning current turn…';
      try {
        const point = await (await api(`/api/chats/${view.chatId}/branch-points`, {
          method:'POST',
          body:'{}'
        })).json();
        await loadBranchPoints(view.chatId, view);
        if (!isCurrentView(view)) return;
        branchPointEl.value = point.node_id;
        branchStatusEl.textContent = `Pinned ${point.message_count} messages as a retained turn.`;
      } catch (error) {
        if (!isCurrentView(view)) return;
        branchStatusEl.textContent = error.message;
        pinBranchBtn.disabled = false;
      }
    }
    async function forkPinnedTurn() {
      const view = currentView();
      let publicationView = view;
      const sourceChat = view.chatId;
      const nodeId = branchPointEl.value;
      if (!sourceChat || !nodeId || isBusy()) return;
      forkBranchBtn.disabled = true;
      branchStatusEl.textContent = 'Creating branch…';
      try {
        const chat = await (await api(`/api/chats/${sourceChat}/forks`, {
          method:'POST',
          body:JSON.stringify({ node_id:nodeId })
        })).json();
        chats.unshift(chat);
        boards.set(chat.id, emptyBoard());
        if (!isCurrentView(view)) {
          renderChats();
          return;
        }
        const selection = selectChat(chat.id);
        publicationView = currentView();
        await selection;
        if (!isCurrentView(publicationView)) return;
        branchStatusEl.textContent = `Branched from ${sourceChat.slice(0, 8)} at a pinned turn.`;
      } catch (error) {
        if (!isCurrentView(publicationView)) return;
        branchStatusEl.textContent = error.message;
        forkBranchBtn.disabled = false;
      }
    }
    async function loadMessages(id, view) {
      if (!isCurrentView(view)) return;
      const requestGeneration = ++messageReadGeneration;
      const messages = await (await api(`/api/chats/${id}/messages`)).json();
      if (!isCurrentView(view) || requestGeneration !== messageReadGeneration
          || (activeRun && isCurrentView(activeRun))) return;
      boards.set(id, emptyBoard());
      messagesEl.innerHTML = '';
      // Replay persisted board snapshots in message order. User messages carry
      // the human move; tool rows carry accepted agent moves.
      const toolsByCallId = new Map();
      const codeLinkedToolIds = new Set();
      for (const message of messages) {
        if (message.kind === 'tool_call' && message.payload?.phase === 'completed' && message.payload.call_id) {
          toolsByCallId.set(message.payload.call_id, message.payload);
        }
        if (message.kind === 'code_block' && message.payload?.phase === 'completed') {
          for (const callId of message.payload.tool_call_ids || []) codeLinkedToolIds.add(callId);
        }
      }
      for (const message of messages) appendMessage(message, view, { toolsByCallId, codeLinkedToolIds });
      renderBoard();
      messagesEl.scrollTop = messagesEl.scrollHeight;
    }
    function appendMessage(message, owner, replay = {}) {
      if (message.kind === 'reasoning') {
        appendReasoningMessage(message.text, owner);
        return;
      }
      if (message.kind === 'tool_call' && message.payload) {
        if (message.payload.phase !== 'completed') return;
        if (message.payload.call_id && replay.codeLinkedToolIds?.has(message.payload.call_id)) return;
        appendTool(message.payload, owner);
        return;
      }
      if (message.kind === 'code_block' && message.payload) {
        if (message.payload.phase !== 'completed') return;
        const linkedTools = (message.payload.tool_call_ids || [])
          .map((callId) => replay.toolsByCallId?.get(callId))
          .filter(Boolean);
        appendCodeBlock(message.payload, owner, linkedTools);
        return;
      }
      if (message.payload?.board?.cells) setBoard(owner, message.payload.board);
      if (!isCurrentView(owner)) return;
      const el = document.createElement('div');
      el.className = `msg ${message.role}`;
      el.innerHTML = `<div class="meta"></div><div></div>`;
      el.querySelector('.meta').textContent = message.role;
      el.lastElementChild.textContent = message.text;
      messagesEl.appendChild(el);
    }
    function appendTool(event, owner, parent = messagesEl) {
      if (event.phase !== 'completed') return;
      applyToolBoard(event, owner);
      if (!isCurrentView(owner)) return;
      const ok = toolSucceeded(event);
      const el = document.createElement('div');
      el.className = 'tool' + (ok ? '' : ' fail');
      el.innerHTML = `<div class="tool-head"><strong></strong><span class="badge"></span><span></span></div><div class="tool-summary"></div><details><summary>JSON payload</summary><pre></pre></details>`;
      el.querySelector('strong').textContent = event.name;
      el.querySelector('.badge').textContent = 'completed';
      el.querySelector('.tool-head span:last-child').textContent = `${ok ? 'ok' : 'failed'} in ${event.duration_ms}ms`;
      const summary = el.querySelector('.tool-summary');
      const raw = toolResult(event);
      if (event.name === 'play_move') {
        const terminal = terminalToolSummary(raw?.board);
        summary.textContent = raw?.accepted
          ? `Agent played O in ${cellName(raw.move?.cell)}. ${terminal || raw.board?.status || ''}`
          : `Move rejected: ${raw?.reason || 'unknown reason'}`;
      } else if (event.name === 'read_board') {
        summary.textContent = `${raw?.status || 'Board read'} · legal moves: ${(raw?.legal_moves || []).join(', ') || 'none'}`;
      } else {
        summary.textContent = 'Tool completed';
      }
      el.querySelector('pre').textContent = JSON.stringify(
        compactToolPayload(event),
        null,
        2
      );
      parent.appendChild(el);
    }
    function appendCodeBlock(event, owner, linkedTools = []) {
      if (event.phase !== 'completed') return;
      for (const tool of linkedTools) applyToolBoard(tool, owner);
      if (!isCurrentView(owner)) return;
      const el = document.createElement('details');
      el.className = 'code-block' + (event.success === false ? ' fail' : '');
      el.open = false;
      el.innerHTML = '<summary></summary><pre></pre>';
      const toolCount = linkedTools.length || (event.tool_call_ids || []).length;
      const toolLabel = toolCount ? ` · ${toolCount} tool${toolCount === 1 ? '' : 's'}` : '';
      const label = `${event.language || 'code'} ${event.success ? 'completed' : 'failed'} in ${event.duration_ms || 0}ms${toolLabel}`;
      el.querySelector('summary').textContent = label;
      const code = event.code || el.querySelector('pre').textContent || '';
      el.querySelector('pre').textContent = code;
      for (const tool of linkedTools) appendTool(tool, owner, el);
      messagesEl.appendChild(el);
      return el;
    }
    function appendCompletedTool(event, run) {
      applyToolBoard(event, run);
      if (run.pendingCodeBlock) {
        run.pendingTools.push(event);
      } else {
        appendTool(event, run);
      }
    }
    function completeCodeBlock(event, run) {
      const linkedIds = new Set(event.tool_call_ids || []);
      const linkedTools = run.pendingTools.filter((tool) => tool.call_id && linkedIds.has(tool.call_id));
      const unlinkedTools = run.pendingTools.filter((tool) => !tool.call_id || !linkedIds.has(tool.call_id));
      appendCodeBlock(
        { ...event, phase:'completed', code: event.code || run.pendingCodeBlock?.code || '' },
        run,
        linkedTools,
      );
      run.pendingCodeBlock = null;
      for (const tool of unlinkedTools) appendTool(tool, run);
      run.pendingTools = [];
    }
    function thinkingPanel(label) {
      const el = document.createElement('details');
      el.className = 'reasoning';
      el.open = true;
      el.innerHTML = '<summary></summary><pre></pre>';
      el.querySelector('summary').textContent = label;
      return el;
    }
    function appendReasoning(delta, run) {
      if (!isCurrentView(run)) return;
      if (!run.reasoning) {
        run.reasoning = thinkingPanel('thinking');
        messagesEl.appendChild(run.reasoning);
      }
      run.reasoning.querySelector('pre').textContent += delta;
    }
    function appendReasoningMessage(text, owner) {
      if (!text || !isCurrentView(owner)) return;
      if (owner.reasoning) {
        owner.reasoning.open = true;
        owner.reasoning.querySelector('summary').textContent = 'thinking';
        owner.reasoning.querySelector('pre').textContent = text;
        owner.reasoning.dataset.persisted = 'true';
        owner.reasoning = null;
        return;
      }
      const el = thinkingPanel('thinking');
      el.querySelector('pre').textContent = text;
      messagesEl.appendChild(el);
    }
    function appendStreamText(delta, run) {
      if (!isCurrentView(run)) return;
      if (!run.streaming) {
        run.streaming = document.createElement('div');
        run.streaming.className = 'msg assistant';
        run.streaming.innerHTML = '<div class="meta">assistant</div><div></div>';
        messagesEl.appendChild(run.streaming);
      }
      run.streaming.lastElementChild.textContent += delta;
    }
    function handleTurnEvent(event, run) {
      if (event.type === 'assistant_prose_delta') appendStreamText(event.text, run);
      if (event.type === 'reasoning_delta') appendReasoning(event.text, run);
      if (event.type === 'code_block_started') run.pendingCodeBlock = event;
      if (event.type === 'code_block_completed') completeCodeBlock(event, run);
      if (event.type === 'tool_call_completed') appendCompletedTool({ ...event, phase:'completed' }, run);
      if (event.type === 'final_value') appendStreamText(renderTerminalValue(event.value), run);
      if (event.type === 'tool_value') appendStreamText(renderTerminalValue(event.value), run);
    }
    function handleObservation(event, run) {
      if (event.type === 'turn_activity') handleTurnEvent(event.activity, run);
    }
    async function sendText(text) {
      if (!text || isBusy()) return;
      if (!activeChat) await newChat();
      if (!activeChat || isBusy()) return;
      ++viewGeneration;
      const run = {
        ...currentView(), streaming:null, reasoning:null, pendingCodeBlock:null,
        pendingTools:[], replayCursor:null, reader:null
      };
      activeRun = run;
      document.querySelector('#text').value = '';
      renderBoard();
      renderBranchPoints();
      let completed = false;
      try {
        const res = await api(`/api/chats/${run.chatId}/messages`, {
          method:'POST',
          headers: {
            'content-type': 'application/json',
            'x-lash-protocol-hello': JSON.stringify({
              negotiation: 'hello',
              supported: { min: 100, max: 100 }
            })
          },
          body: JSON.stringify({ text, board:boards.get(run.chatId), ...selectedModel() })
        });
        const accept = JSON.parse(res.headers.get('x-lash-protocol-accept') || 'null');
        if (accept?.negotiation !== 'accept' || accept.selected !== 100
            || accept.supported?.min > 100 || accept.supported?.max < 100) {
          throw new Error('remote protocol negotiation failed');
        }
        run.reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buffer = '';
        while (true) {
          const { value, done } = await run.reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream:true });
          const lines = buffer.split('\n');
          buffer = lines.pop();
          for (const line of lines) {
            if (!line.trim()) continue;
            const item = JSON.parse(line);
            if (item.type === 'message') appendMessage(item.message, run);
            if (item.type === 'replay_cursor') run.replayCursor = item.cursor;
            if (item.type === 'replay_gap') run.replayCursor = item.gap?.latest_cursor ?? run.replayCursor;
            if (item.type === 'observation') handleObservation(item.event, run);
            if (item.type === 'error' && isCurrentView(run)) alert(item.message);
            if (isCurrentView(run)) messagesEl.scrollTop = messagesEl.scrollHeight;
          }
        }
        completed = true;
      } catch (error) {
        if (isCurrentView(run)) alert(error.message);
      } finally {
        try {
          for (const tool of run.pendingTools) appendTool(tool, run);
        } finally {
          run.streaming = null;
          run.reasoning = null;
          run.pendingCodeBlock = null;
          run.pendingTools = [];
          try {
            run.reader?.releaseLock();
          } finally {
            run.reader = null;
            if (activeRun === run) {
              activeRun = null;
              renderBoard();
              renderBranchPoints();
            }
          }
        }
      }
      if (completed) await loadChats(activeChat === run.chatId ? currentView() : run);
    }
    async function send(e) {
      e.preventDefault();
      await sendText(document.querySelector('#text').value.trim());
    }
    document.querySelector('#newChat').onclick = newChat;
    document.querySelector('#mobileNew').onclick = newChat;
    document.querySelector('#resetBoard').onclick = resetBoard;
    pinBranchBtn.onclick = pinCurrentTurn;
    forkBranchBtn.onclick = forkPinnedTurn;
    modelInput.addEventListener('change', saveActiveModel);
    variantInput.addEventListener('change', saveActiveModel);
    document.querySelector('#text').addEventListener('keydown', (event) => {
      if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
        event.preventDefault();
        form.requestSubmit();
      }
    });
    form.onsubmit = send;
    renderBoard();
    renderBranchPoints();
    loadSettings().then(() => loadChats()).then(() => { if (!activeChat) newChat(); else renderBoard(); });
  </script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    #[test]
    fn browser_stream_ownership() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/browser_stream_ownership.mjs");
        let node =
            std::env::var_os("LASH_AGENT_SERVICE_TEST_NODE").unwrap_or_else(|| "node".into());
        let output = std::process::Command::new(node)
            .arg("--test")
            .arg("--test-reporter=tap")
            .arg(script)
            .output()
            .expect("run agent-service browser ownership fixtures");
        println!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(
            output.status.success(),
            "browser ownership fixtures failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
