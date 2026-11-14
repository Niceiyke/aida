param(
    [string]$Ffmpeg = 'ffmpeg',
    [string]$Ffprobe = 'ffprobe',
    [string]$Font = "$env:WINDIR\Fonts\arial.ttf"
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$demoId = [guid]::NewGuid().ToString('N')
$demoRoot = Join-Path $repoRoot "target\demo-$demoId"
$workspace = Join-Path $demoRoot 'workspace'
New-Item -ItemType Directory -Force -Path $demoRoot | Out-Null

function Invoke-Checked {
    param([string]$Executable, [string[]]$Arguments)
    & $Executable @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Executable failed with exit code $LASTEXITCODE"
    }
}

if (-not (Test-Path -LiteralPath $Font -PathType Leaf)) {
    throw "Font file not found: $Font"
}

Invoke-Checked 'cargo' @('build', '--release', '--manifest-path', (Join-Path $repoRoot 'Cargo.toml'))
$studio = Join-Path $repoRoot 'target\release\studio.exe'
$videoA = Join-Path $demoRoot 'clip-a.mp4'
$videoB = Join-Path $demoRoot 'clip-b.mp4'
$audio = Join-Path $demoRoot 'music.wav'
$voice = Join-Path $demoRoot 'voice-placeholder.wav'

Invoke-Checked $Ffmpeg @('-hide_banner', '-v', 'error', '-f', 'lavfi', '-i', 'color=c=0x17324D:s=640x360:r=30:d=7.5', '-an', '-c:v', 'libx264', '-pix_fmt', 'yuv420p', '-y', $videoA)
Invoke-Checked $Ffmpeg @('-hide_banner', '-v', 'error', '-f', 'lavfi', '-i', 'color=c=0x6B2737:s=640x360:r=30:d=7.5', '-an', '-c:v', 'libx264', '-pix_fmt', 'yuv420p', '-y', $videoB)
Invoke-Checked $Ffmpeg @('-hide_banner', '-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=440:sample_rate=48000:duration=15', '-c:a', 'pcm_s16le', '-y', $audio)
# A tone stands in for narration so the demo remains self-contained and uses no generated or licensed voice.
Invoke-Checked $Ffmpeg @('-hide_banner', '-v', 'error', '-f', 'lavfi', '-i', 'sine=frequency=660:sample_rate=48000:duration=2', '-c:a', 'pcm_s16le', '-y', $voice)

$assetA = (& $studio import --workspace $workspace --file $videoA --ffprobe $Ffprobe | Out-String | ConvertFrom-Json)
if ($LASTEXITCODE -ne 0) { throw 'Import of first video failed' }
$assetB = (& $studio import --workspace $workspace --file $videoB --ffprobe $Ffprobe | Out-String | ConvertFrom-Json)
if ($LASTEXITCODE -ne 0) { throw 'Import of second video failed' }
$assetAudio = (& $studio import --workspace $workspace --file $audio --ffprobe $Ffprobe | Out-String | ConvertFrom-Json)
if ($LASTEXITCODE -ne 0) { throw 'Import of music failed' }
$assetVoice = (& $studio import --workspace $workspace --file $voice --ffprobe $Ffprobe | Out-String | ConvertFrom-Json)
if ($LASTEXITCODE -ne 0) { throw 'Import of narration placeholder failed' }

$assets = @{}
$assets[$assetA.assetId] = @{ kind = $assetA.kind; uri = $assetA.uri; sha256 = $assetA.sha256 }
$assets[$assetB.assetId] = @{ kind = $assetB.kind; uri = $assetB.uri; sha256 = $assetB.sha256 }
$assets[$assetAudio.assetId] = @{ kind = $assetAudio.kind; uri = $assetAudio.uri; sha256 = $assetAudio.sha256 }
$assets[$assetVoice.assetId] = @{ kind = $assetVoice.kind; uri = $assetVoice.uri; sha256 = $assetVoice.sha256 }

$timeline = @{
    schemaVersion = '1.0.0'
    projectId = '018f2e40-4d28-7e31-8b63-611d49c43b0a'
    revision = 0
    canvas = @{ width = 1920; height = 1080; fps = @{ num = 30; den = 1 }; background = '#101820' }
    assets = $assets
    tracks = @(
        @{
            id = 'video-main'; kind = 'video'; order = 0; enabled = $true; locked = $false
            clips = @(
                @{ id = 'clip-a'; kind = 'video'; assetId = $assetA.assetId; startFrame = 0; durationFrames = 225; sourceIn = @{ ticks = 0; timebase_num = 1; timebase_den = 30 }; sourceOut = @{ ticks = 225; timebase_num = 1; timebase_den = 30 }; transform = @{ x = 0; y = 0; scaleX = 1; scaleY = 1; rotationDeg = 0; opacity = 1 }; effects = @() },
                @{ id = 'clip-b'; kind = 'video'; assetId = $assetB.assetId; startFrame = 225; durationFrames = 225; sourceIn = @{ ticks = 0; timebase_num = 1; timebase_den = 30 }; sourceOut = @{ ticks = 225; timebase_num = 1; timebase_den = 30 }; transform = @{ x = 0; y = 0; scaleX = 1; scaleY = 1; rotationDeg = 0; opacity = 1 }; effects = @() }
            )
        },
        @{
            id = 'music'; kind = 'audio'; order = 1; enabled = $true; locked = $false
            clips = @(@{ id = 'music-bed'; kind = 'audio'; assetId = $assetAudio.assetId; startFrame = 0; durationFrames = 450; sourceIn = @{ ticks = 0; timebase_num = 1; timebase_den = 48000 }; sourceOut = @{ ticks = 720000; timebase_num = 1; timebase_den = 48000 }; gain = 0.2 })
        },
        @{
            id = 'narration'; kind = 'audio'; order = 2; enabled = $true; locked = $false
            clips = @(@{ id = 'voice-placeholder'; kind = 'audio'; assetId = $assetVoice.assetId; startFrame = 60; durationFrames = 60; sourceIn = @{ ticks = 0; timebase_num = 1; timebase_den = 48000 }; sourceOut = @{ ticks = 96000; timebase_num = 1; timebase_den = 48000 }; gain = 0.6 })
        },
        @{
            id = 'titles'; kind = 'overlay'; order = 3; enabled = $true; locked = $false
            clips = @(@{ id = 'title'; kind = 'text'; startFrame = 20; durationFrames = 120; text = 'Avida Studio'; fontSize = 64; color = '#FFFFFF'; x = 80; y = 250 })
        }
    )
    settings = @{ audioSampleRate = 48000; colorSpace = 'rec709'; exportPreset = 'landscape_1080p30' }
}

$timelinePath = Join-Path $demoRoot 'timeline.json'
$timeline | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $timelinePath -Encoding utf8
$output = Join-Path $demoRoot 'promo.mp4'
Invoke-Checked $studio @('render', '--workspace', $workspace, '--timeline', $timelinePath, '--output', $output, '--ffmpeg', $Ffmpeg, '--ffprobe', $Ffprobe, '--font', $Font)
Write-Host "Demo video: $output"
Write-Host "Provenance: $output.provenance.json"
