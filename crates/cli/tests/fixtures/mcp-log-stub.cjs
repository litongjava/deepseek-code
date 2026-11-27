// A Streamable HTTP MCP server with observable start/stop: one `start` line
// when it answers the first initialize, one `stop` line when the manager ends
// the session with DELETE. It also reports its ephemeral port so the test can
// write the user configuration.
const fs = require('node:fs');
const http = require('node:http');

const log = process.env.MCP_LOG;
const portFile = process.env.MCP_PORT_FILE;
let started = false;

const server = http.createServer((request, response) => {
  if (request.method === 'DELETE') {
    fs.appendFileSync(log, 'stop\n');
    response.writeHead(200, {'mcp-session-id': 'stub-session'});
    response.end();
    return;
  }
  let body = '';
  request.on('data', chunk => body += chunk);
  request.on('end', () => {
    let message;
    try {
      message = JSON.parse(body);
    } catch {
      response.writeHead(400);
      response.end('invalid JSON');
      return;
    }
    const messages = Array.isArray(message) ? message : [message];
    const replies = [];
    let initialized = false;
    for (const item of messages) {
      if (item.id === undefined) continue;
      let result = {};
      if (item.method === 'initialize') {
        initialized = true;
        result = {protocolVersion: '2024-11-05', capabilities: {tools: {}, resources: {}}, serverInfo: {name: 'stub', version: '1'}};
      } else if (item.method === 'tools/list') {
        result = {tools: [{name: 'probe', description: 'stub tool', inputSchema: {type: 'object'}}]};
      } else if (item.method === 'resources/list') {
        result = {resources: []};
      } else if (item.method === 'prompts/list') {
        result = {prompts: []};
      } else if (item.method === 'tools/call') {
        result = {content: [{type: 'text', text: 'ok'}]};
      }
      replies.push({jsonrpc: '2.0', id: item.id, result});
    }
    if (initialized && !started) {
      started = true;
      fs.appendFileSync(log, 'start\n');
    }
    if (replies.length === 0) {
      response.writeHead(202);
      response.end();
      return;
    }
    const headers = {'content-type': 'application/json'};
    if (initialized) headers['mcp-session-id'] = 'stub-session';
    response.writeHead(200, headers);
    response.end(JSON.stringify(replies.length === 1 ? replies[0] : replies));
  });
});

server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync(portFile, String(server.address().port));
});
