
$ErrorActionPreference = 'Stop'
if ([Console]::IsOutputRedirected -or [Console]::IsInputRedirected) { throw 'CONSOLE_REDIRECTED' }
[Console]::WriteLine('CONSOLE_OK')
[Console]::WriteLine(('SIZE:{0}:{1}' -f [Console]::WindowHeight,[Console]::WindowWidth))
$child = Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 600' -NoNewWindow -PassThru
[IO.File]::WriteAllText((Join-Path $pwd 'descendant.pid'), $child.Id.ToString())
[IO.File]::WriteAllText((Join-Path $pwd 'shell.pid'), $PID.ToString())
[Console]::WriteLine('READY')
while ($null -ne ($line = [Console]::ReadLine())) {
    [IO.File]::AppendAllText((Join-Path $pwd 'received-lines'), $line + "`n")
    [Console]::WriteLine('echo:' + $line)
    [Console]::WriteLine(('SIZE:{0}:{1}' -f [Console]::WindowHeight,[Console]::WindowWidth))
    if ($line -eq 'exit') { break }
}
