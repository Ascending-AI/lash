
import json, os, signal, sys, threading, time

# The mock ignores SIGTERM so scripted tests deterministically reach the
# SIGKILL stage of forced shutdown; graceful stdin-EOF exit is unchanged.
signal.signal(signal.SIGTERM, signal.SIG_IGN)

lock = threading.Lock()
behavior = os.environ['BEHAVIOR']
protocol = os.environ.get('PROTOCOL', '2025-11-25')
log_path = os.environ['LOG_PATH']
starts_path = os.environ['STARTS_PATH']
pid_path = os.environ.get('PID_PATH')
eof_path = os.environ.get('EOF_PATH')
close_path = os.environ.get('CLOSE_PATH')
if pid_path:
    with open(pid_path, 'w', encoding='utf-8') as f:
        f.write(str(os.getpid()))

try:
    with open(starts_path, 'r', encoding='utf-8') as f:
        starts = int(f.read())
except (FileNotFoundError, ValueError):
    starts = 0
with open(starts_path, 'w', encoding='utf-8') as f:
    f.write(str(starts + 1))
if behavior == 'fail_once_then_success' and starts < 1:
    sys.exit(1)
if behavior == 'fail_twice_then_success' and starts < 2:
    sys.exit(1)
if behavior == 'reset_attempts_after_success' and starts in (0, 2):
    sys.exit(1)

def send(message):
    with lock:
        sys.stdout.write(json.dumps(message, separators=(',', ':')))
        sys.stdout.flush()
        sys.stdout.write('\n')
        sys.stdout.flush()

def result(request_id):
    send({'jsonrpc': '2.0', 'id': request_id,
          'result': {'content': [{'type': 'text', 'text': 'ok'}]}})

def run_call(message, index):
    request_id = message['id']
    token = message.get('params', {}).get('_meta', {}).get('progressToken')
    if behavior == 'success':
        result(request_id)
    elif behavior == 'rpc_invalid_params':
        send({'jsonrpc': '2.0', 'id': request_id, 'error': {
            'code': -32602, 'message': 'bad field', 'data': {'field': 'query', 'expected': 'string'}}})

call_index = 0
list_index = 0
for line in sys.stdin:
    with open(log_path, 'a', encoding='utf-8') as log:
        log.write(line)
    message = json.loads(line)
    method = message.get('method')
    if method == 'initialize' and behavior not in ('hang_initialize', 'exit_on_eof_after_hang_initialize'):
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
            'protocolVersion': protocol,
            'capabilities': {'tools': {}},
            'serverInfo': {'name': 'policy-mock', 'version': '1.0.0'}}})
    elif method == 'tools/list':
        list_index += 1
        if behavior == 'stall_refresh' and list_index > 1:
            continue
        if behavior == 'cursor_cycle':
            send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
                'tools': [], **({'nextCursor': 'cycle'} if list_index < 3 else {})}})
            continue
        if behavior == 'boundary':
            page = int(message.get('params', {}).get('cursor', '0'))
            send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
                'tools': [{'name': 'tool-' + str(page * 64 + i), 'inputSchema': {'type': 'object'}} for i in range(64)],
                **({'nextCursor': str(page + 1)} if page < 63 else {})}})
            continue
        if behavior == 'page_limit':
            page = int(message.get('params', {}).get('cursor', '0'))
            send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
                'tools': [], **({'nextCursor': str(page + 1)} if page < 64 else {})}})
            continue
        if behavior == 'item_limit':
            page = int(message.get('params', {}).get('cursor', '0'))
            tools = [{'name': 'tool-' + str(page * 64 + i), 'inputSchema': {'type': 'object'}}
                     for i in range(65 if page == 63 else 64)]
            send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
                'tools': tools, **({'nextCursor': str(page + 1)} if page < 63 else {})}})
            continue
        if behavior == 'byte_limit':
            tools = [{'name': 'tool-' + str(list_index), 'inputSchema': {'type': 'object'},
                      'description': 'x' * (4 * 1024 * 1024 + 1)}]
            send({'jsonrpc': '2.0', 'id': message['id'], 'result': {
                'tools': tools, **({'nextCursor': 'second'} if list_index == 1 else {})}})
            continue
        tool_name = 'work'
        if behavior == 'refresh_version':
            tool_name = 'work-' + str(list_index)
        if behavior == 'catalog_by_generation':
            tool_name = 'generation-' + str(starts + 1)
        send({'jsonrpc': '2.0', 'id': message['id'], 'result': {'tools': [{
            'name': tool_name, 'description': 'Policy test tool',
            'inputSchema': {'type': 'object', 'properties': {}}}]}})
        if behavior == 'exit_after_list' or (behavior == 'exit_after_list_once' and starts < 1):
            sys.exit(0)
        if behavior == 'reset_attempts_after_success' and starts == 1:
            sys.exit(0)
        if behavior == 'close_streams_when_triggered_after_list':
            while not os.path.exists(close_path):
                time.sleep(0.001)
            os.close(sys.stdin.fileno())
            os.close(sys.stdout.fileno())
            time.sleep(30)
    elif method == 'tools/call':
        call_index += 1
        if behavior == 'crash_after_call':
            result(message['id'])
            sys.exit(0)
        else:
            threading.Thread(target=run_call, args=(message, call_index), daemon=True).start()
    elif method == 'ping':
        if behavior in ('silent_ping', 'success', 'fail_twice_then_success'):
            send({'jsonrpc': '2.0', 'id': message['id'], 'result': {}})
if behavior in ('ignore_eof', 'exit_on_eof_after_hang_initialize'):
    with open(eof_path, 'w', encoding='utf-8') as f:
        f.write('closed')
if behavior == 'ignore_eof':
    time.sleep(30)
