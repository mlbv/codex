"""Run fixtures as a standard Windows user outside the runner's process jobs."""

import os
from pathlib import Path
import subprocess
import sys


TASK_SCRIPT = r"""
$ErrorActionPreference = 'Stop'
& wevtutil sl Microsoft-Windows-TaskScheduler/Operational /e:true
if ($LASTEXITCODE -ne 0) { throw 'Cannot enable task activation diagnostics.' }
$registeredAt = Get-Date
$service = New-Object -ComObject 'Schedule.Service'
$service.Connect()
$folder = $service.GetFolder('\')
$name = 'CodexParity-' + [Guid]::NewGuid().ToString('N')
$definition = $service.NewTask(0)
$definition.Principal.UserId = "$env:COMPUTERNAME\$env:DAEMON_PARITY_USER"
$definition.Principal.LogonType = 1
$definition.Principal.RunLevel = 0
$definition.Settings.AllowDemandStart = $true
$definition.Settings.Enabled = $true
$definition.Settings.DisallowStartIfOnBatteries = $false
$definition.Settings.StopIfGoingOnBatteries = $false
$definition.Settings.ExecutionTimeLimit = 'PT10M'
$action = $definition.Actions.Create(0)
$action.Path = $env:DAEMON_PARITY_PYTHON
$action.Arguments = $env:DAEMON_PARITY_ARGUMENTS
$action.WorkingDirectory = $env:DAEMON_PARITY_TEMP
# Password logon and least privilege give the task its own standard-user token.
$registered = $folder.RegisterTaskDefinition(
    $name, $definition, 2, $definition.Principal.UserId,
    $env:DAEMON_PARITY_PASSWORD, 1, $null
)
try {
    $registered.Run($null) | Out-Null
    $timer = [Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        $current = $folder.GetTask($name)
        $result = $current.LastTaskResult
        if ($current.State -notin @(2, 4) -and $result -ne 0x41303) {
            break
        }
        if ($timer.Elapsed.TotalSeconds -gt 600) {
            throw 'Parity worker exceeded its ten-minute limit.'
        }
        if ($timer.Elapsed.TotalSeconds -gt 60 -and -not (Test-Path -LiteralPath $env:DAEMON_PARITY_LOG)) {
            throw 'Task Scheduler did not start the parity worker within one minute.'
        }
        Start-Sleep -Milliseconds 200
    }
    if ($result -ne 0) {
        throw ('Standard-user parity task exited with status 0x{0:x8}' -f $result)
    }
} finally {
    $current = $folder.GetTask($name)
    $events = @(Get-WinEvent -FilterHashtable @{
        LogName = 'Microsoft-Windows-TaskScheduler/Operational'
        StartTime = $registeredAt
    } -ErrorAction SilentlyContinue | Where-Object { $_.Message -like "*$name*" } | ForEach-Object {
        @{ id = $_.Id; message = $_.Message }
    })
    [ordered]@{
        task = $name; enabled = $current.Enabled; state = $current.State
        result = $current.LastTaskResult; definition = $current.Xml; events = $events
    } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $env:DAEMON_PARITY_DIAGNOSTICS -Encoding utf8
    $registered.Stop(0)
    $folder.DeleteTask($name, 0)
}
"""


def run_fixture(config_path: Path, log_path: Path):
    if (
        os.environ.get("GITHUB_ACTIONS") != "true"
        or os.environ.get("RUNNER_ENVIRONMENT") != "github-hosted"
    ):
        raise RuntimeError(
            "Parity account execution is restricted to GitHub-hosted runners"
        )
    command = [
        "-X",
        "utf8",
        "-u",
        str(Path(__file__).with_name("daemon_release.py")),
        "fixture-worker",
        "--worker-config",
        str(config_path),
        "--output",
        str(log_path.parent),
    ]
    environment = dict(os.environ)
    # Task Scheduler supplies the worker's profile/environment rather than inheriting
    # the runner's token or credentials. pythonw keeps the background task windowless.
    environment["DAEMON_PARITY_PYTHON"] = str(
        Path(sys.executable).with_name("pythonw.exe")
    )
    environment["DAEMON_PARITY_ARGUMENTS"] = subprocess.list2cmdline(command)
    environment["DAEMON_PARITY_LOG"] = str(log_path)
    diagnostics = log_path.with_suffix(".task.json")
    environment["DAEMON_PARITY_DIAGNOSTICS"] = str(diagnostics)
    completed = subprocess.run(
        ["pwsh", "-NoProfile", "-NonInteractive", "-Command", TASK_SCRIPT],
        env=environment,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=660,
        creationflags=subprocess.CREATE_NO_WINDOW,
    )
    if log_path.exists():
        print(log_path.read_text(encoding="utf-8"), end="", flush=True)
    if completed.returncode:
        if diagnostics.exists():
            print(diagnostics.read_text(encoding="utf-8"), flush=True)
        raise RuntimeError(
            "Standard-user parity task failed:\n" + completed.stdout + completed.stderr
        )
