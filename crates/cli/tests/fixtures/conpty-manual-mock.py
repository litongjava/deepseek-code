"""Local-only OpenAI fixture for manual CLI/Desktop ConPTY acceptance (no credentials).
Run: python conpty-manual-mock.py --port 18765 --log manual-model.jsonl
Prompts: CONPTY start / get / write / resize / eof / stop / normal / denied
"""
import argparse
import json
import os
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

parser = argparse.ArgumentParser()
parser.add_argument('--port', type=int, default=18765)
parser.add_argument('--log', required=True)
args = parser.parse_args()
log = open(args.log, 'x', encoding='utf-8')
lock = threading.Lock()


def actions(prompt):
    load = ('ToolSearch', {'query': 'select:TaskInput,TaskGet,TaskStop'})
    read = ('TaskGet', {'task_id': '$task', 'block': True, 'timeout': 1000})
    if 'get' in prompt:
        return [load, read]
    if 'start' in prompt or 'denied' in prompt:
        command = "& ([scriptblock]::Create([IO.File]::ReadAllText((Join-Path $pwd 'fixture.ps1'),[Text.Encoding]::UTF8)))" if 'start' in prompt else "[IO.File]::WriteAllText((Join-Path $pwd 'DENIED'),'bad')"
        return [load, ('Bash', {'command': command, 'shell': 'powershell', 'interaction': 'pty', 'run_in_background': True})] + ([read] if 'start' in prompt else [])
    if 'resize' in prompt:
        return [load, ('TaskInput', {'task_id': '$task', 'resize': {'rows': 35, 'cols': 110}}), ('TaskInput', {'task_id': '$task', 'data': 'after-resize\r'}), read]
    if 'eof' in prompt:
        return [load, ('TaskInput', {'task_id': '$task', 'data': 'MUST_NOT_WRITE\r', 'eof': True}), read]
    if 'normal' in prompt:
        return [load, ('TaskInput', {'task_id': '$task', 'data': 'exit\r'}), ('TaskGet', {'task_id': '$task', 'block': True, 'timeout': 20000})]
    if 'stop' in prompt:
        return [load, ('TaskStop', {'task_id': '$task'}), ('TaskGet', {'task_id': '$task', 'block': True, 'timeout': 20000})]
    return [load, ('TaskInput', {'task_id': '$task', 'data': 'manual-input\r'}), read]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        # Desktop provider/model discovery, still entirely local and synthetic.
        encoded = json.dumps({'object': 'list', 'data': [{'id': 'fixture', 'object': 'model', 'owned_by': 'local-fixture'}]}).encode('utf-8')
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        messages = body['messages']
        users = [i for i, m in enumerate(messages) if m.get('role') == 'user']
        # Background task notifications can be represented as user messages.
        # Keep the explicit operator step instead of starting a new action plan.
        steps = [i for i in users if 'conpty ' in str(messages[i].get('content', '')).lower()]
        start = (steps or users)[-1]
        content = messages[start].get('content', '')
        prompt = (content if isinstance(content, str) else json.dumps(content)).lower()
        task = None
        for message in messages:
            if message.get('role') == 'tool':
                try:
                    result = json.loads(message.get('content', ''))
                    task = result.get('task_id', task)
                except (ValueError, AttributeError):
                    pass
        calls = actions(prompt)
        completed = sum(m.get('role') == 'tool' for m in messages[start + 1:])
        if completed < len(calls):
            name, parameters = calls[completed]
            if parameters.get('task_id') == '$task':
                parameters['task_id'] = task or 'missing-start-task'
            call = {'index': 0, 'id': 'manual-' + uuid.uuid4().hex, 'type': 'function', 'function': {'name': name, 'arguments': json.dumps(parameters)}}
            delta, reason = {'tool_calls': [call]}, 'tool_calls'
        else:
            delta, reason = {'content': 'Local ConPTY fixture turn finished. Inspect tool results and saved evidence.'}, 'stop'
        with lock:
            # No headers/config/environment are logged. All messages come from
            # the isolated manual fixture; never point real accounts at this.
            log.write(json.dumps({'request': body, 'delta': delta}, ensure_ascii=False) + '\n')
            log.flush()
        frames = [{'choices': [{'index': 0, 'delta': delta, 'finish_reason': None}]}, {'choices': [{'index': 0, 'delta': {}, 'finish_reason': reason}]}]
        response = ''.join('data: ' + json.dumps(frame) + '\n\n' for frame in frames) + 'data: [DONE]\n\n'
        encoded = response.encode('utf-8')
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)


print(f'Local fixture listening on http://127.0.0.1:{args.port}/v1 (PID {os.getpid()})', flush=True)
ThreadingHTTPServer(('127.0.0.1', args.port), Handler).serve_forever()
