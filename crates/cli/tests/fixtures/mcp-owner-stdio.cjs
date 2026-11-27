// Independent witness for process ownership. A forced kill cannot run a JS
// exit handler, so each successor checks the prior PID before logging startup.
const fs = require("node:fs");
const readline = require("node:readline");
const [log, id] = process.argv.slice(2);
if (process.argv[4] === "survive-eof") setInterval(() => {}, 60000);
const history = fs.existsSync(log) ? fs.readFileSync(log, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse) : [];
const overlap = history.some(row => {
  if (row.id !== id) return false;
  try {
    process.kill(row.pid, 0);
    return true;
  } catch (error) {
    if (error.code !== "ESRCH") throw error;
    return false;
  }
});
fs.appendFileSync(log, JSON.stringify({id, pid: process.pid, launcher: process.ppid, owner: Number(process.env.MCP_FIXTURE_OWNER || process.ppid), overlap}) + "\n");

readline.createInterface({input: process.stdin}).on("line", line => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  let result;
  switch (request.method) {
    case "initialize":
      result = {protocolVersion: "2025-06-18", serverInfo: {name: id, version: "1"}, capabilities: {tools: {}, prompts: {}}};
      break;
    case "tools/list": {
      const field = process.env.PROBE_FIELD;
      const inputSchema = field ? {type: "object", properties: {[field]: {type: "string"}}, required: [field]} : {type: "object"};
      result = {tools: [{name: "probe", description: field || "owner witness", inputSchema}]};
      break;
    }
    case "resources/list": result = {resources: []}; break;
    case "prompts/list": result = {prompts: [{name: "probe", arguments: []}]}; break;
    case "prompts/get": result = {messages: [{role: "user", content: {type: "text", text: "expanded owner probe"}}]}; break;
    default: result = {};
  }
  console.log(JSON.stringify({jsonrpc: "2.0", id: request.id, result}));
});
