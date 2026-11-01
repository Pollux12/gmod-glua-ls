param(
    [switch]$Serve,
    [switch]$Status,
    [string]$WorkingDirectory = (Get-Location).Path,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$SamplyArguments
)

$ErrorActionPreference = 'Stop'
$pipeName = 'gluals-samply-bridge'

function Connect-SamplyBridge([string]$request, [int]$timeout) {
    $pipe = [IO.Pipes.NamedPipeClientStream]::new('.', $pipeName, [IO.Pipes.PipeDirection]::InOut)
    try {
        $pipe.Connect($timeout)
        $writer = [IO.StreamWriter]::new($pipe)
        $writer.AutoFlush = $true
        $reader = [IO.StreamReader]::new($pipe)
        $writer.WriteLine($request)
        $reader.ReadLine() | ConvertFrom-Json
    } finally {
        $pipe.Dispose()
    }
}

if ($Serve) {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Start the bridge from an Administrator PowerShell terminal.'
    }

    $samply = (Get-Command samply -CommandType Application).Source
    $pipeSecurity = [IO.Pipes.PipeSecurity]::new()
    $pipeSecurity.AddAccessRule([IO.Pipes.PipeAccessRule]::new(
        $identity.User,
        [IO.Pipes.PipeAccessRights]::ReadWrite,
        [Security.AccessControl.AccessControlType]::Allow
    ))
    Write-Host "Samply bridge running with $samply"
    Write-Host 'Press Ctrl+C or close this terminal to stop it.'

    $previousControlCMode = [Console]::TreatControlCAsInput
    [Console]::TreatControlCAsInput = $true
    try {
        :BridgeLoop while ($true) {
            $pipe = [IO.Pipes.NamedPipeServerStreamAcl]::Create(
                $pipeName,
                [IO.Pipes.PipeDirection]::InOut,
                1,
                [IO.Pipes.PipeTransmissionMode]::Byte,
                [IO.Pipes.PipeOptions]::Asynchronous,
                0,
                0,
                $pipeSecurity,
                [IO.HandleInheritability]::None,
                [IO.Pipes.PipeAccessRights]0
            )
            try {
                $connectionCancellation = [Threading.CancellationTokenSource]::new()
                $connection = $pipe.WaitForConnectionAsync($connectionCancellation.Token)
                $stopRequested = $false
                while (-not $connection.IsCompleted) {
                    if ([Console]::KeyAvailable) {
                        $key = [Console]::ReadKey($true)
                        if ($key.Key -eq [ConsoleKey]::C -and
                            ($key.Modifiers -band [ConsoleModifiers]::Control)) {
                            $stopRequested = $true
                            $connectionCancellation.Cancel()
                            break
                        }
                    }
                    Start-Sleep -Milliseconds 100
                }
                try {
                    $connection.GetAwaiter().GetResult()
                } catch [OperationCanceledException] {
                    if (-not $stopRequested) {
                        throw
                    }
                }
                if ($stopRequested) {
                    break BridgeLoop
                }
                $reader = [IO.StreamReader]::new($pipe)
                $writer = [IO.StreamWriter]::new($pipe)
                $writer.AutoFlush = $true
                $request = $reader.ReadLine() | ConvertFrom-Json

                if ($request.Action -eq 'Status') {
                    $writer.WriteLine((@{ Running = $true; Samply = $samply } | ConvertTo-Json -Compress))
                    continue
                }

                if ($request.Action -ne 'Run') {
                    throw "Unknown bridge action: $($request.Action)"
                }

                $output = @()
                $exitCode = 1
                Push-Location -LiteralPath $request.WorkingDirectory
                try {
                    $output = @(& $samply @($request.Arguments) 2>&1 | ForEach-Object { $_.ToString() })
                    $exitCode = $LASTEXITCODE
                } catch {
                    $output += $_.Exception.Message
                } finally {
                    Pop-Location
                }

                $writer.WriteLine((@{ ExitCode = $exitCode; Output = $output } | ConvertTo-Json -Compress))
            } catch {
                if ($pipe.IsConnected) {
                    $writer.WriteLine((@{ ExitCode = 1; Output = @($_.Exception.Message) } | ConvertTo-Json -Compress))
                }
            } finally {
                if ($connectionCancellation) {
                    $connectionCancellation.Dispose()
                }
                $pipe.Dispose()
            }
        }
    } finally {
        if ($pipe) {
            $pipe.Dispose()
        }
        [Console]::TreatControlCAsInput = $previousControlCMode
        Write-Host 'Samply bridge stopped.'
    }
    exit 0
}

if ($Status) {
    try {
        $response = Connect-SamplyBridge (@{ Action = 'Status' } | ConvertTo-Json -Compress) 500
        Write-Host "Running: $($response.Samply)"
        exit 0
    } catch {
        Write-Host 'Not running'
        exit 1
    }
}

if (-not $SamplyArguments) {
    throw 'Pass Samply arguments, or use -Serve or -Status.'
}

$resolvedWorkingDirectory = (Resolve-Path -LiteralPath $WorkingDirectory).Path
$request = @{
    Action = 'Run'
    WorkingDirectory = $resolvedWorkingDirectory
    Arguments = @($SamplyArguments)
} | ConvertTo-Json -Compress

try {
    $response = Connect-SamplyBridge $request 2000
} catch {
    throw 'The Samply bridge is not running. Start it from an Administrator terminal with tools/samply-bridge.ps1 -Serve.'
}

$response.Output | ForEach-Object { Write-Host $_ }
exit [int]$response.ExitCode
