<#
.SYNOPSIS
  Install the PdfCraft Arabic MSI, check what it installed, launch the installed app and CLI, then
  uninstall it and check nothing is left behind. Meant for disposable CI runners (it installs
  per-machine and needs administrator rights).

.DESCRIPTION
  1. The scope guard rejects a per-user install (#305).
  2. A silent per-machine install puts pdfcraft.exe, pdfcraft-cli.exe and the licence texts in
     Program Files\PdfCraft Arabic, plus Start menu and desktop shortcuts and the Open-with
     registration (PdfCraftArabic.Document).
  3. The installed CLI opens, reads, edits, renders and saves a small PDF.
  4. The installed app starts with the UI control channel and answers a screenshot request, so it
     really drew a window.
  5. A silent uninstall removes the program, the shortcuts and the registration.
  Writes logs, the CLI outputs and the screenshot to .\smoke for upload.

.EXAMPLE
  pwsh packaging/windows/smoke-msi.ps1 -Msi dist\pdfcraft-arabic-0.4.0-windows-x64.msi
#>
param([Parameter(Mandatory)] [string] $Msi)
$ErrorActionPreference = 'Stop'
$Msi = (Resolve-Path -LiteralPath $Msi).Path
$Smoke = Join-Path $PWD 'smoke'
New-Item -ItemType Directory -Force $Smoke | Out-Null

function Invoke-Msiexec([string[]] $Arguments, [int[]] $Expect = @(0), [string] $Log) {
  $all = $Arguments + @('/l*v', (Join-Path $Smoke $Log))
  $p = Start-Process msiexec.exe -WindowStyle Hidden -ArgumentList $all -Wait -PassThru
  if ($Expect -notcontains $p.ExitCode) {
    Get-Content (Join-Path $Smoke $Log) -Tail 60
    throw "msiexec $($Arguments -join ' ') exited $($p.ExitCode), expected $($Expect -join ' or ')"
  }
}

$Dir = Join-Path $env:ProgramFiles 'PdfCraft Arabic'
$Exe = Join-Path $Dir 'pdfcraft.exe'
$Cli = Join-Path $Dir 'pdfcraft-cli.exe'
$Shortcuts = @(
  (Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'PdfCraft Arabic.lnk'),
  (Join-Path ([Environment]::GetFolderPath('CommonDesktopDirectory')) 'PdfCraft Arabic.lnk')
)
$ProgId = 'HKLM:\Software\Classes\PdfCraftArabic.Document'
$Capabilities = 'HKLM:\Software\PdfCraftArabic\Capabilities'

# 1. Per-user installs are refused by the scope guard (exit 1603 with its message).
Invoke-Msiexec @('/i', "`"$Msi`"", '/qn', 'ALLUSERS=2', 'MSIINSTALLPERUSER=1') -Expect @(1603) -Log 'per-user.log'
if (-not (Select-String -Path (Join-Path $Smoke 'per-user.log') -SimpleMatch 'PdfCraft Arabic must be installed for all users.')) {
  throw 'per-user install did not fail on the scope guard'
}
Write-Output 'ok per-user install refused'

# 2. Per-machine install.
Invoke-Msiexec @('/i', "`"$Msi`"", '/qn') -Log 'install.log'
foreach ($f in $Exe, $Cli, (Join-Path $Dir 'licenses\LICENSE-MIT.txt'), (Join-Path $Dir 'licenses\LICENSE-APACHE.txt'),
    (Join-Path $Dir 'licenses\NOTICE.txt'), (Join-Path $Dir 'licenses\FONT-LICENSES.txt')) {
  if (-not (Test-Path -LiteralPath $f)) { throw "installer did not create $f" }
}
# (PowerShell variable names ignore case: the loop variable must not be called $exe.)
foreach ($bin in $Exe, $Cli) {
  $bytes = [System.IO.File]::ReadAllBytes($bin)
  $machine = [BitConverter]::ToUInt16($bytes, [BitConverter]::ToInt32($bytes, 0x3C) + 4)
  if ($machine -ne 0x8664) { throw "$bin is for machine 0x$('{0:X}' -f $machine), not x64" }
}
if (-not (Select-String -Path (Join-Path $Dir 'licenses\NOTICE.txt') -SimpleMatch 'modified version of PdfCraft')) {
  throw 'installed NOTICE does not say this is a modified version'
}
$wsh = New-Object -ComObject WScript.Shell
foreach ($shortcut in $Shortcuts) {
  if (-not (Test-Path -LiteralPath $shortcut)) { throw "installer did not create $shortcut" }
  $target = $wsh.CreateShortcut($shortcut).TargetPath
  if ($target -ne $Exe) { throw "$shortcut points to '$target', not $Exe" }
}
foreach ($key in $ProgId, $Capabilities) {
  if (-not (Test-Path $key)) { throw "installer did not register $key" }
}
$name = (Get-ItemProperty $Capabilities).ApplicationName
if ($name -ne 'PdfCraft Arabic') { throw "Capabilities ApplicationName is '$name'" }
$product = Get-CimInstance Win32_Product -Filter "Name = 'PdfCraft Arabic'" -ErrorAction SilentlyContinue
if ($product -and $product.Vendor -ne 'PdfCraft Arabic contributors') { throw "MSI vendor is '$($product.Vendor)'" }
Write-Output "ok installed into $Dir (x64 binaries, licences, shortcuts, Open-with registration)"

# 3. The installed CLI opens, reads, edits, renders and saves a PDF.
$version = & $Cli --version | Out-String
if ($LASTEXITCODE -ne 0 -or $version -notmatch 'PdfCraft Arabic') { Write-Output $version; throw 'pdfcraft-cli --version failed' }
$body = 'BT /F1 24 Tf 20 150 Td (Hello Windows) Tj ET'
$objs = @('<< /Type /Catalog /Pages 2 0 R >>', '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
  '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>',
  "<< /Length $($body.Length) >>`nstream`n$body`nendstream", '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>')
$sb = [System.Text.StringBuilder]::new("%PDF-1.7`n"); $offsets = @()
for ($i = 0; $i -lt $objs.Count; $i++) { $offsets += $sb.Length; [void]$sb.Append("$($i + 1) 0 obj`n$($objs[$i])`nendobj`n") }
$xref = $sb.Length
[void]$sb.Append("xref`n0 $($objs.Count + 1)`n0000000000 65535 f `n")
foreach ($o in $offsets) { [void]$sb.Append(('{0:D10} 00000 n `n' -f $o)) }
[void]$sb.Append("trailer`n<< /Size $($objs.Count + 1) /Root 1 0 R >>`nstartxref`n$xref`n%%EOF`n")
[System.IO.File]::WriteAllText((Join-Path $Smoke 'hello.pdf'), $sb.ToString(), [System.Text.Encoding]::ASCII)
$steps = '[{"tool":"doc_open","args":{"path":"hello.pdf"}},{"tool":"text_paragraphs","args":{"doc":1,"page":1}},{"tool":"text_edit","args":{"doc":1,"page":1,"paragraph":1,"text":"Edited on Windows"}},{"tool":"page_render","args":{"doc":1,"page":1,"dpi":72},"out":"page.png"},{"tool":"doc_save","args":{"doc":1,"path":"out.pdf"}}]'
[System.IO.File]::WriteAllText((Join-Path $Smoke 'steps.json'), $steps)
$out = & $Cli run --script (Join-Path $Smoke 'steps.json') --root $Smoke 2>&1 | Out-String
Set-Content -LiteralPath (Join-Path $Smoke 'cli-run.txt') -Value $out
if ($LASTEXITCODE -ne 0) { Write-Output $out; throw "pdfcraft-cli run exited $LASTEXITCODE" }
if ($out -notmatch 'Hello Windows') { Write-Output $out; throw 'text_paragraphs did not read the page' }
foreach ($f in 'page.png', 'out.pdf') {
  $p = Join-Path $Smoke $f
  if (-not (Test-Path $p) -or (Get-Item $p).Length -eq 0) { Write-Output $out; throw "$f was not written" }
}
Write-Output 'ok installed pdfcraft-cli: opened, read, edited, rendered and saved a PDF'

# 3b. Arabic: added as shaped text in an embedded font, saved, reopened, extracted and found.
$arabic = '[{"tool":"doc_open","args":{"path":"hello.pdf"}},{"tool":"page_add_text","args":{"doc":1,"page":1,"text":"مرحبا بالعالم","at":[20,40],"width":260,"size":16}},{"tool":"doc_save","args":{"doc":1,"path":"arabic.pdf"}},{"tool":"doc_open","args":{"path":"arabic.pdf"}},{"tool":"text_extract","args":{"doc":2}},{"tool":"text_find","args":{"doc":2,"query":"مرحبا"}},{"tool":"page_render","args":{"doc":2,"page":1,"dpi":96},"out":"arabic.png"}]'
[System.IO.File]::WriteAllText((Join-Path $Smoke 'arabic.json'), $arabic, [System.Text.UTF8Encoding]::new($false))
$out = & $Cli run --script (Join-Path $Smoke 'arabic.json') --root $Smoke 2>&1 | Out-String
Set-Content -LiteralPath (Join-Path $Smoke 'cli-arabic.txt') -Value $out -Encoding utf8
if ($LASTEXITCODE -ne 0) { Write-Output $out; throw "pdfcraft-cli run (Arabic) exited $LASTEXITCODE" }
if ($out -notmatch 'مرحبا' -or $out -notmatch 'بالعالم') { Write-Output $out; throw 'the Arabic text was not extracted back' }
if ($out -notmatch '"count":\s*1') { Write-Output $out; throw 'text_find did not find the Arabic word' }
# (The embedded CIDFontType2/FontFile2/ToUnicode structure is checked by the Rust tests; a full
# save packs those dictionaries into compressed object streams, so they aren't visible here.)
foreach ($f in 'arabic.pdf', 'arabic.png') {
  $p = Join-Path $Smoke $f
  if (-not (Test-Path $p) -or (Get-Item $p).Length -eq 0) { Write-Output $out; throw "$f was not written" }
}
Write-Output 'ok installed pdfcraft-cli: Arabic added, embedded, saved, extracted and found'

# 3c. Arabic PDF to Word: the saved Arabic PDF exports to a .docx whose text is the Arabic
# letters (no unreadable characters), right to left, in one section for the one page, with the
# PDF's font named (not embedded).
$export = '[{"tool":"doc_open","args":{"path":"arabic.pdf"}},{"tool":"doc_export_office","args":{"doc":1,"path":"arabic.docx"}}]'
[System.IO.File]::WriteAllText((Join-Path $Smoke 'export.json'), $export, [System.Text.UTF8Encoding]::new($false))
$out = & $Cli run --script (Join-Path $Smoke 'export.json') --root $Smoke 2>&1 | Out-String
Set-Content -LiteralPath (Join-Path $Smoke 'cli-export.txt') -Value $out -Encoding utf8
if ($LASTEXITCODE -ne 0) { Write-Output $out; throw "pdfcraft-cli run (Word export) exited $LASTEXITCODE" }
if ($out -notmatch '"unreadable_chars":\s*0') { Write-Output $out; throw 'the Word export has unreadable characters' }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [System.IO.Compression.ZipFile]::OpenRead((Join-Path $Smoke 'arabic.docx'))
try {
  $part = $zip.GetEntry('word/document.xml')
  if (-not $part) { throw 'arabic.docx has no word/document.xml' }
  $reader = [System.IO.StreamReader]::new($part.Open(), [System.Text.Encoding]::UTF8)
  $xml = $reader.ReadToEnd()
  $reader.Dispose()
  $embedded = @($zip.Entries | Where-Object { $_.FullName -like 'word/fonts/*' })
} finally {
  $zip.Dispose()
}
[void][xml]$xml
if ($xml -notmatch 'مرحبا' -or $xml -notmatch 'بالعالم') { throw 'arabic.docx lacks the Arabic text' }
if ($xml -notmatch '<w:bidi/>' -or $xml -notmatch '<w:rtl/>') { throw 'arabic.docx is not marked right to left' }
if (([regex]::Matches($xml, '<w:sectPr>')).Count -ne 1) { throw 'arabic.docx should have one section for the one page' }
if ($xml -notmatch 'w:cs="Noto Sans Arabic"') { throw 'arabic.docx does not name the PDF font' }
if ($embedded.Count -ne 0) { throw 'arabic.docx embeds fonts' }
Write-Output 'ok installed pdfcraft-cli: Arabic PDF exported to Word (letters, right to left, one section, font named)'

# 4. The installed app starts and draws its window (answers a screenshot over the control channel).
$control = Join-Path $Smoke 'control.json'
$app = Start-Process -FilePath $Exe -ArgumentList '--control', "`"$control`"", "`"$(Join-Path $Smoke 'hello.pdf')`"" -PassThru
try {
  $deadline = (Get-Date).AddSeconds(90)
  $shot = Join-Path $Smoke 'app-window.png'
  $ok = $false
  while ((Get-Date) -lt $deadline) {
    if ($app.HasExited) { throw "pdfcraft.exe exited early with code $($app.ExitCode)" }
    if (Test-Path -LiteralPath $control) {
      & $Cli ui --control $control screenshot --out $shot 2>&1 | Out-String | Set-Content (Join-Path $Smoke 'ui-screenshot.txt')
      if ($LASTEXITCODE -eq 0 -and (Test-Path $shot) -and (Get-Item $shot).Length -gt 0) { $ok = $true; break }
    }
    Start-Sleep -Seconds 2
  }
  if (-not $ok) { throw 'the installed app did not answer a screenshot request within 90 s' }
  Write-Output "ok installed pdfcraft.exe launched and drew its window ($shot)"
} finally {
  if (-not $app.HasExited) { Stop-Process -Id $app.Id -Force; $app.WaitForExit(15000) | Out-Null }
}

# 5. Uninstall removes everything the installer created.
Invoke-Msiexec @('/x', "`"$Msi`"", '/qn') -Log 'uninstall.log'
foreach ($f in @($Exe, $Cli) + $Shortcuts) {
  if (Test-Path -LiteralPath $f) { throw "uninstall left $f behind" }
}
foreach ($key in $ProgId, $Capabilities) {
  if (Test-Path $key) { throw "uninstall left $key behind" }
}
if (Test-Path -LiteralPath $Dir) {
  $left = Get-ChildItem -Recurse -Force -LiteralPath $Dir | Select-Object -ExpandProperty FullName
  if ($left) { throw "uninstall left files in ${Dir}: $($left -join ', ')" }
}
Write-Output 'ok uninstalled: no files, shortcuts or registration left'
