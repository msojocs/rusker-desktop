# Regenerates the PNG sizes and rusker.ico from rusker-1024.png.
# rusker-1024.png is rendered from rusker.svg, e.g. with Edge:
#   msedge --headless=new --hide-scrollbars --default-background-color=00000000 `
#     --window-size=1024,1024 --screenshot=<dir>\rusker-1024.png file:///<dir>/rusker.svg
param([string]$Dir = $PSScriptRoot)
Add-Type -AssemblyName System.Drawing

function Resize([System.Drawing.Image]$src, [int]$n) {
  $bmp = New-Object System.Drawing.Bitmap $n, $n, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.InterpolationMode = 'HighQualityBicubic'; $g.SmoothingMode = 'HighQuality'
  $g.PixelOffsetMode = 'HighQuality'; $g.CompositingQuality = 'HighQuality'
  $g.DrawImage($src, 0, 0, $n, $n); $g.Dispose()
  $bmp
}

# 32bpp DIB icon entry: BITMAPINFOHEADER + bottom-up BGRA + AND mask.
function DibBytes([System.Drawing.Bitmap]$bmp) {
  $n = $bmp.Width
  $ms = New-Object IO.MemoryStream; $w = New-Object IO.BinaryWriter $ms
  $maskStride = [int]([math]::Ceiling($n / 32) * 4)
  $w.Write([UInt32]40); $w.Write([Int32]$n); $w.Write([Int32]($n * 2))
  $w.Write([UInt16]1); $w.Write([UInt16]32); $w.Write([UInt32]0)
  $w.Write([UInt32]($n * $n * 4 + $maskStride * $n))
  $w.Write([Int32]0); $w.Write([Int32]0); $w.Write([UInt32]0); $w.Write([UInt32]0)
  $rect = New-Object System.Drawing.Rectangle 0, 0, $n, $n
  $data = $bmp.LockBits($rect, 'ReadOnly', [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $px = New-Object byte[] ($data.Stride * $n)
  [Runtime.InteropServices.Marshal]::Copy($data.Scan0, $px, 0, $px.Length)
  $bmp.UnlockBits($data)
  for ($y = $n - 1; $y -ge 0; $y--) { $w.Write($px, $y * $data.Stride, $n * 4) }
  $w.Write((New-Object byte[] ($maskStride * $n)))
  ,$ms.ToArray()
}

$src = [System.Drawing.Image]::FromFile((Join-Path $Dir 'rusker-1024.png'))
$entries = @{}
foreach ($n in 512, 256, 128, 64, 48, 32, 16) {
  $bmp = Resize $src $n
  $png = Join-Path $Dir "rusker-$n.png"
  $bmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
  $entries[$n] = [byte[]]$(if ($n -ge 256) { [IO.File]::ReadAllBytes($png) } else { DibBytes $bmp })
  $bmp.Dispose()
}
$src.Dispose()

$icoSizes = 256, 128, 64, 48, 32, 16
$ms = New-Object IO.MemoryStream; $w = New-Object IO.BinaryWriter $ms
$w.Write([UInt16]0); $w.Write([UInt16]1); $w.Write([UInt16]$icoSizes.Count)
$offset = 6 + 16 * $icoSizes.Count
foreach ($n in $icoSizes) {
  [byte[]]$d = $entries[$n]; $b = if ($n -ge 256) { 0 } else { $n }
  $w.Write([byte]$b); $w.Write([byte]$b); $w.Write([byte]0); $w.Write([byte]0)
  $w.Write([UInt16]1); $w.Write([UInt16]32); $w.Write([UInt32]$d.Length); $w.Write([UInt32]$offset)
  $offset += $d.Length
}
foreach ($n in $icoSizes) { $w.Write([byte[]]$entries[$n]) }
[IO.File]::WriteAllBytes((Join-Path $Dir 'rusker.ico'), $ms.ToArray())
