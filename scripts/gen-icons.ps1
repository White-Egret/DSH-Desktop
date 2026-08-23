# Regenerate all DSH-Launcher icons from the root whale PNG.
# The source is the whale icon-128x128.png directly under D:\DSH.
# Usage: powershell -ExecutionPolicy Bypass -File scripts\gen-icons.ps1
$ErrorActionPreference = 'Stop'

$srcPath = (Get-ChildItem -LiteralPath 'D:\DSH' -File -Filter '*icon-128x128.png' | Select-Object -First 1).FullName
if (-not $srcPath) { throw 'whale source PNG not found under D:\DSH' }
$outDir  = Join-Path $PSScriptRoot '..\src-tauri\icons'

Add-Type -AssemblyName System.Drawing

$src = [System.Drawing.Image]::FromFile($srcPath)

function Save-ResizedPng([int]$size, [string]$name) {
    $bmp = New-Object System.Drawing.Bitmap($src, $size, $size)
    $path = Join-Path $outDir $name
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
    Write-Host "Generated $path"
}

# 1) PNG icons (used by Tauri bundle and default window icon)
Save-ResizedPng 32  '32x32.png'
Copy-Item -LiteralPath $srcPath -Destination (Join-Path $outDir '128x128.png') -Force
Save-ResizedPng 256 '128x128@2x.png'
Save-ResizedPng 512 'icon.png'

# 2) icon.ico: Windows desktop / taskbar / tray exe icon.
#    Small sizes use 32bpp BMP entries (good compatibility), all sizes BMP for maximum compatibility.
$icoSizes = @(16, 24, 32, 48, 64, 128, 256)
$images = @()

function New-BmpIcoImage([int]$size, [System.Drawing.Bitmap]$bitmap) {
    $rect = New-Object System.Drawing.Rectangle(0, 0, $size, $size)
    $data = $bitmap.LockBits(
        $rect,
        [System.Drawing.Imaging.ImageLockMode]::ReadOnly,
        [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    )
    try {
        $stride = [Math]::Abs($data.Stride)
        $pixels = New-Object byte[] ($stride * $size)
        [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $pixels, 0, $pixels.Length)

        $xor = New-Object byte[] ($size * $size * 4)
        for ($y = 0; $y -lt $size; $y++) {
            $srcRow = ($size - 1 - $y) * $stride
            [Array]::Copy($pixels, $srcRow, $xor, $y * $size * 4, $size * 4)
        }

        $andRowBytes = [int]([Math]::Ceiling($size / 32.0) * 4)
        $andMask = New-Object byte[] ($andRowBytes * $size)

        $ms = New-Object System.IO.MemoryStream
        $bw = New-Object System.IO.BinaryWriter($ms)
        # BITMAPINFOHEADER
        $bw.Write([uint32]40)
        $bw.Write([int32]$size)
        $bw.Write([int32]($size * 2))
        $bw.Write([uint16]1)
        $bw.Write([uint16]32)
        $bw.Write([uint32]0)
        $bw.Write([uint32]($xor.Length + $andMask.Length))
        $bw.Write([int32]0) # x pixels per meter
        $bw.Write([int32]0) # y pixels per meter
        $bw.Write([uint32]0) # colors used
        $bw.Write([uint32]0) # important colors
        $bw.Write($xor)
        $bw.Write($andMask)
        $bw.Flush()
        return [byte[]]$ms.ToArray()
    } finally {
        $bitmap.UnlockBits($data)
    }
}

foreach ($size in $icoSizes) {
    $bmp = New-Object System.Drawing.Bitmap($src, $size, $size)
    try {
        $images += ,(New-BmpIcoImage $size $bmp)
    } finally {
        $bmp.Dispose()
    }
}

$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)
$bw.Write([uint16]0)          # reserved
$bw.Write([uint16]1)          # type = icon
$bw.Write([uint16]$icoSizes.Count)
$offset = 6 + 16 * $icoSizes.Count
for ($i = 0; $i -lt $icoSizes.Count; $i++) {
    $size = $icoSizes[$i]
    $bytes = $images[$i]
    $dim = if ($size -eq 256) { 0 } else { $size }
    $bw.Write([byte]$dim)   # width
    $bw.Write([byte]$dim)   # height
    $bw.Write([byte]0)   # color count
    $bw.Write([byte]0)   # reserved
    $bw.Write([uint16]1) # planes
    $bw.Write([uint16]32)# bit count
    $bw.Write([uint32]$bytes.Length)
    $bw.Write([uint32]$offset)
    $offset += $bytes.Length
}
foreach ($bytes in $images) {
    $bw.Write([byte[]]$bytes)
}
$bw.Flush()
$icoPath = Join-Path $outDir 'icon.ico'
[System.IO.File]::WriteAllBytes($icoPath, $ms.ToArray())
Write-Host "Generated $icoPath"

$src.Dispose()
Write-Host 'Icon generation complete.'
