param(
    [string]$OutputDirectory = (Join-Path $PSScriptRoot "media"),
    [string]$Ffmpeg = "ffmpeg"
)

$ErrorActionPreference = "Stop"

try {
    & $Ffmpeg -version | Out-Null
} catch {
    throw "FFmpeg was not found. Add ffmpeg.exe to PATH or pass -Ffmpeg 'C:\path\ffmpeg.exe'."
}

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null

function Invoke-Encode {
    param([string]$Name, [string[]]$Arguments)
    $target = Join-Path $OutputDirectory $Name
    Write-Host "Generating $Name ..."
    & $Ffmpeg @Arguments $target
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path $target) -or (Get-Item $target).Length -eq 0) {
        throw "FFmpeg failed while generating $Name (exit code $LASTEXITCODE). Check that the required encoder is enabled."
    }
}

$commonVideo = @(
    "-hide_banner", "-y",
    "-f", "lavfi", "-i", "testsrc2=size=3840x2160:rate=30",
    "-t", "12",
    "-an"
)

Invoke-Encode "yoru_4k_h264_long_gop.mp4" ($commonVideo + @(
    "-c:v", "libx264", "-preset", "medium", "-crf", "19",
    "-pix_fmt", "yuv420p", "-g", "240", "-keyint_min", "240",
    "-sc_threshold", "0", "-movflags", "+faststart"
))

Invoke-Encode "yoru_4k_hevc_main10.mp4" ($commonVideo + @(
    "-c:v", "libx265", "-preset", "medium", "-crf", "22",
    "-pix_fmt", "yuv420p10le", "-x265-params", "keyint=240:min-keyint=240:scenecut=0",
    "-tag:v", "hvc1", "-movflags", "+faststart"
))

Invoke-Encode "yoru_4k_prores_422.mov" ($commonVideo + @(
    "-c:v", "prores_ks", "-profile:v", "2", "-pix_fmt", "yuv422p10le"
))

Write-Host "Benchmark media generated in: $OutputDirectory"
