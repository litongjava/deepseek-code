// A local stdio server with observable process identity and a gated call.
const fs = require('node:fs');
const lines = require('node:readline').createInterface({input: process.stdin});
lines.on('line', async line => {
  const {id, method, params} = JSON.parse(line);
  if (id === undefined) return;
  let result = {};
  if (method === 'initialize') {
    result = {protocolVersion: '2024-11-05', capabilities: {tools: {}}, serverInfo: {name: 'probe', version: '1'}};
  } else if (method === 'tools/list') {
    result = {tools: [{name: 'probe', description: process.env.REVISION, inputSchema: {type: 'object'}}]};
  } else if (method === 'tools/call') {
    if (params.arguments.wait) {
      fs.writeFileSync(process.env.GATE + '.waiting', '');
      while (!fs.existsSync(process.env.GATE)) await new Promise(resolve => setTimeout(resolve, 10));
    }
    result = {content: [{type: 'text', text: String(process.pid)}]};
  } else if (method === 'resources/list') result = {resources: []};
  else if (method === 'prompts/list') result = {prompts: []};
  process.stdout.write(JSON.stringify({jsonrpc: '2.0', id, result}) + '\n');
});
