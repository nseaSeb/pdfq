# PDFQ terminal demo. Regenerate from the repo root: vhs docs/demo/demo.tape
# Needs Docker, curl, jq, pdftotext/pdfinfo (poppler) and host port 3100 free.

Output docs/demo/pdfq-demo.gif

Require docker
Require curl
Require jq
Require pdftotext

Set Shell bash
Set FontSize 20
Set Width 1200
Set Height 700
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 30ms
Set Framerate 24

Hide
Type "export PS1='$ ' REPO=$PWD; docker rm -f pdfq-demo >/dev/null 2>&1; docker run -d --rm --name pdfq-demo -p 3100:3000 -v $REPO/examples:/templates:ro ghcr.io/nseaseb/pdfq:0.1.0-typst0.15.1 >/dev/null" Enter
Type "until curl -fsS localhost:3100/health >/dev/null 2>&1; do sleep 0.5; done" Enter
Type `cd $(mktemp -d) && cp $REPO/examples/invoice/data.json . && echo '#read("/etc/passwd")' > evil.typ && cp $REPO/tests/fixtures/hostile/doubling-array.typ bomb.typ && clear` Enter
Sleep 1s
Show

Type "# 1. Compile a Typst file (multipart upload + an input)" Enter
Type@12ms `echo '= Hello #sys.inputs.at("who", default: "you")' > main.typ` Enter
Type@12ms `curl -sS -F file=@main.typ -F input.who=LinkedIn localhost:3100/compile -o hello.pdf -w '%{http_code} %{size_download} bytes\n'` Enter
Sleep 1s
Type "pdftotext hello.pdf -" Enter
Sleep 2s
Type@1ms "clear" Enter

Type "# 2. Render a server-side template with JSON data" Enter
Type@12ms `curl -sS localhost:3100/compile -H 'content-type: application/json' -d "{\"template\": \"invoice\", \"data\": $(cat data.json)}" -o invoice.pdf -w '%{http_code} %{size_download} bytes\n'` Enter
Sleep 1s
Type "pdfinfo invoice.pdf | head -3" Enter
Sleep 2s
Type@1ms "clear" Enter

Type "# 3. Untrusted documents cannot read the host" Enter
Type "cat evil.typ" Enter
Sleep 500ms
Type@12ms "curl -sS -F 'file=@evil.typ;filename=main.typ' localhost:3100/compile | jq .error" Enter
Sleep 2500ms
Type@1ms "clear" Enter

Type "# 4. A memory bomb gets a typed error, not a dead server" Enter
Type "cat bomb.typ" Enter
Sleep 1s
Type@12ms "curl -sS -F 'file=@bomb.typ;filename=main.typ' localhost:3100/compile | jq .error" Enter
Sleep 2500ms
Type "# 5. Still healthy" Enter
Type "curl -s localhost:3100/health | jq -c" Enter
Sleep 3s

Hide
Type "docker stop pdfq-demo >/dev/null" Enter
Sleep 2s
