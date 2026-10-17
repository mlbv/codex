$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'Parity account setup is restricted to GitHub-hosted runners.'
}

$workspacePath = [IO.Path]::GetFullPath($env:GITHUB_WORKSPACE)
$suffix = $env:GITHUB_RUN_ID.Substring([Math]::Max(0, $env:GITHUB_RUN_ID.Length - 6))
$parityName = "codexci$suffix"
$parityPassword = [Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(24)) + 'aA1!'
Write-Output "::add-mask::$parityPassword"
$parityUser = New-LocalUser -Name $parityName -Password (ConvertTo-SecureString $parityPassword -AsPlainText -Force) -PasswordNeverExpires
Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $parityUser

$paritySid = $parityUser.SID.Value
$rightsExport = Join-Path $env:RUNNER_TEMP 'daemon-parity-user-rights.inf'
$rightsGrant = Join-Path $env:RUNNER_TEMP 'daemon-parity-batch-logon.inf'
$rightsDatabase = Join-Path $env:RUNNER_TEMP 'daemon-parity-batch-logon.sdb'
& secedit /export /cfg $rightsExport /areas USER_RIGHTS /quiet | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot read the runner user-rights policy.' }
$batchRight = @(Get-Content -LiteralPath $rightsExport | Where-Object { $_ -match '^SeBatchLogonRight\s*=' })
if ($batchRight.Count -ne 1) { throw 'Expected one batch-logon right in the runner policy.' }
@(
    '[Unicode]'
    'Unicode=yes'
    '[Version]'
    'signature="$CHICAGO$"'
    'Revision=1'
    '[Privilege Rights]'
    ($batchRight[0] + ",*$paritySid")
) | Set-Content -LiteralPath $rightsGrant -Encoding unicode
& secedit /configure /db $rightsDatabase /cfg $rightsGrant /areas USER_RIGHTS /quiet | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot grant the parity account batch-logon permission.' }

$parityTemp = Join-Path ([IO.Path]::GetPathRoot($workspacePath)) "dp-$suffix"
$parityOutput = Join-Path $workspacePath 'artifacts\parity'
New-Item -ItemType Directory -Path $parityTemp, $parityOutput -Force | Out-Null

& icacls $workspacePath /grant "*${paritySid}:(OI)(CI)RX" /T /Q | Out-Null
if ($LASTEXITCODE -ne 0) { throw 'Cannot grant parity account read access to the checkout.' }
foreach ($parityDirectory in @($parityTemp, $parityOutput)) {
    & icacls $parityDirectory /grant "*${paritySid}:(OI)(CI)M" /T /Q | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Cannot grant parity account access to $parityDirectory" }
}

@(
    "DAEMON_PARITY_USER=$parityName"
    "DAEMON_PARITY_PASSWORD=$parityPassword"
    "DAEMON_PARITY_TEMP=$parityTemp"
) | Out-File -LiteralPath $env:GITHUB_ENV -Encoding utf8 -Append
Write-Output "Parity fixtures will run as standard user $parityName."
