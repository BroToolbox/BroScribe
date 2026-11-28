$ErrorActionPreference = 'Stop'
$helper = Join-Path (Split-Path -Parent $PSScriptRoot) 'src/uninstall-after-exit.ps1'
. $helper
$crates = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$shapePath = Join-Path $crates 'broscribe-platform/src/profile_cleanup.json'
$shapes = [IO.File]::ReadAllText($shapePath) | ConvertFrom-Json
$fixture = [IO.File]::ReadAllText((Join-Path $PSScriptRoot 'fixtures/cleanup-cases.json')) | ConvertFrom-Json
foreach ($case in $fixture.cases) {
  $shape = $shapes.($case.shape)
  $actual = if ($case.type -eq 'pattern') {
    Matches-Shape $case.path @($case.pattern)
  } elseif ($case.type -eq 'literal') {
    Matches-CleanupRule $case.path @{ literal = $true; files = @($case.pattern) } 'file'
  } else {
    Matches-CleanupRule $case.path $shape $case.type
  }
  if ([bool]$actual -ne $case.matches) {
    throw ("{0} {1} {2}: expected {3}, got {4} (pattern {5})" -f
      $case.shape, $case.type, $case.path, $case.matches, $actual, $case.pattern)
  }
}

$temp = Join-Path ([IO.Path]::GetTempPath()) ('broscribe-cleanup-' + [Guid]::NewGuid())
[void][IO.Directory]::CreateDirectory($temp)
try {
  $temp = [IO.Path]::GetFullPath($temp)
  foreach ($evidence in $fixture.evidence) {
    $root = Join-Path $temp $evidence.name
    [void][IO.Directory]::CreateDirectory($root)
    foreach ($relative in $evidence.dirs) {
      [void][IO.Directory]::CreateDirectory((Join-Path $root $relative))
    }
    foreach ($relative in $evidence.files) {
      $path = Join-Path $root $relative
      [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path))
      [IO.File]::WriteAllText($path, 'synthetic file')
    }
    $detected = $false
    try { Assert-NoLibraryHere $root }
    catch {
      if (!$_.Exception.Message.StartsWith('The removal path contains a BroScribe library at ')) { throw }
      $detected = $true
    }
    if ($detected -ne $evidence.detected) { throw ('Unexpected library evidence for ' + $evidence.name) }
  }

  $model = Join-Path $temp 'test-model'
  $partial = Join-Path $model '.partial'
  [void][IO.Directory]::CreateDirectory($partial)
  [IO.File]::WriteAllText((Join-Path $partial '.entries.json'),
    '{"id":"test-model","files":["assets/model.onnx"],"dirs":["assets"],"sources":["model.tar.bz2"]}')
  $staged = Read-ModelShape $model
  foreach ($path in @('.partial/assets/model.onnx', '.partial/model.tar.bz2')) {
    if (!(Matches-CleanupRule $path $staged 'file')) { throw ('Missing staged file ' + $path) }
  }
  foreach ($path in @('assets/model.onnx', 'model.tar.bz2')) {
    if (Matches-CleanupRule $path $staged 'file') { throw ('Claimed root file before install ' + $path) }
  }
  [IO.File]::WriteAllText((Join-Path $partial '.entries.json'),
    '{"id":"test-model","files":["assets/model.onnx"],"dirs":["assets"],"sources":["model.tar.bz2"],"promoting":true}')
  $promoted = Read-ModelShape $model
  foreach ($path in @('assets/model.onnx', '.partial/assets/model.onnx')) {
    if (!(Matches-CleanupRule $path $promoted 'file')) { throw ('Missing promoted file ' + $path) }
  }
  if (Matches-CleanupRule 'model.tar.bz2' $promoted 'file') { throw 'Claimed the archive at the model root' }

  $profile = Join-Path $temp 'orphan-profile'
  $journal = Join-Path $profile 'journal'
  [void][IO.Directory]::CreateDirectory((Join-Path $journal '.discard'))
  $sessionPath = Join-Path $journal 'synthetic-session'
  foreach ($track in @('mic', 'system')) {
    $trackPath = Join-Path $journal ('synthetic-session/' + $track)
    [void][IO.Directory]::CreateDirectory($trackPath)
    $env:BROSCRIBE_UNINSTALL_DISCARD_UNSAVED = '0'
    [IO.File]::WriteAllText((Join-Path $trackPath 'notes.f32'), 'synthetic foreign file')
    [IO.File]::WriteAllText((Join-Path $trackPath '0000001.f32'), 'noncanonical sequence')
    Assert-NoUnsavedJournals $profile
    $chunk = Join-Path $trackPath '000000.f32'
    [IO.File]::WriteAllBytes($chunk, [BitConverter]::GetBytes([single]0.25))
    $blocked = $false
    try { Assert-NoUnsavedJournals $profile }
    catch {
      if (!$_.Exception.Message.Contains($sessionPath) -or
          $_.Exception.Message -notlike '*Move this folder away*--remove-data --discard-unsaved*') { throw }
      $blocked = $true
    }
    if (!$blocked) { throw 'An orphaned chunk did not block cleanup' }
    $env:BROSCRIBE_UNINSTALL_DISCARD_UNSAVED = '1'
    Assert-NoUnsavedJournals $profile
    [IO.File]::Delete($chunk)
  }
  $kept = [Collections.Generic.List[string]]::new()
  $keptParents = [Collections.Generic.List[string]]::new()
  Remove-Owned $profile 'data' @()
  if (!($kept | Where-Object { [IO.Path]::GetFileName($_) -ceq 'notes.f32' })) {
    throw 'Foreign audio-looking files were not kept and reported'
  }

  $outside = Join-Path $temp 'outside-journal'
  [void][IO.Directory]::CreateDirectory($outside)
  $linkProfile = Join-Path $temp 'linked-profile'
  $linkJournal = Join-Path $linkProfile 'journal'
  [void][IO.Directory]::CreateDirectory($linkProfile)
  $linkKind = if ([Environment]::OSVersion.Platform -eq 'Win32NT') { 'Junction' } else { 'SymbolicLink' }
  foreach ($link in @($linkJournal, (Join-Path $linkJournal 'linked-session'))) {
    [void][IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($link))
    [void](New-Item -ItemType $linkKind -Path $link -Target $outside)
    try {
      $blocked = $false
      try { Assert-NoUnsavedJournals $linkProfile }
      catch {
        if (!$_.Exception.Message.StartsWith('Cannot verify journals at ') -or
            !$_.Exception.Message.Contains($link)) { throw }
        $blocked = $true
      }
      if (!$blocked) { throw ('Journal link did not block cleanup: ' + $link) }
    } finally {
      [IO.Directory]::Delete($link, $false)
    }
  }
  Remove-Item Env:BROSCRIBE_UNINSTALL_DISCARD_UNSAVED

  $receipt = Join-Path $temp 'receipt.txt'
  $line = ([string][char]0x00E9) * 1000
  Write-CleanupReceipt $receipt 'Done. Removed 1 entries.' ((,$line) * 400)
  if ([IO.File]::ReadAllText($receipt) -cne 'Done. Removed 1 entries.') { throw 'Receipt status contains details' }
  $detailPath = $receipt + '.details'
  if ([IO.FileInfo]::new($detailPath).Length -gt 65536) { throw 'Receipt details exceed the reader bound' }
  $details = [IO.File]::ReadAllText($detailPath)
  if ($details -cnotmatch 'Omitted 368 additional detail lines\.') { throw 'Incorrect receipt omission count' }

  if ([Environment]::OSVersion.Platform -eq 'Win32NT') {
    $reference = Join-Path $temp 'BroScribe.uninstall-helper.json'
    $otherReceipt = Join-Path $temp 'other-receipt.txt'
    $other = $otherReceipt | ConvertTo-Json -Compress
    [IO.File]::WriteAllText($reference, $other)
    if ((Remove-HelperReference $reference $receipt) -or [IO.File]::ReadAllText($reference) -cne $other) {
      throw 'A reference to another receipt was reported or removed'
    }
    [IO.File]::WriteAllText($reference, 'invalid JSON')
    $result = Remove-HelperReference $reference $receipt
    if (!$result.kept.StartsWith("kept: $reference (could not be checked: ") -or
        [IO.File]::ReadAllText($reference) -cne 'invalid JSON') {
      throw 'An invalid receipt reference was removed or left without an explanation'
    }
    $blockedReference = [IO.Path]::Combine($reference, 'nested.json')
    $result = Remove-HelperReference $blockedReference $receipt
    if (!$result.kept.StartsWith("kept: $blockedReference (could not be checked: ")) {
      throw 'An invalid reference parent was left without an explanation'
    }
    [IO.File]::WriteAllText($reference, (('\\?\' + $receipt) | ConvertTo-Json -Compress))
    $result = Remove-HelperReference ('\\?\' + $reference) $receipt
    if ($result.removed -cne $reference -or [IO.File]::Exists($reference)) {
      throw 'Matching receipt reference was not removed and reported in plain form'
    }
    if (Remove-HelperReference $reference $receipt) { throw 'Missing receipt reference was removed' }
  }
} finally {
  [IO.Directory]::Delete($temp, $true)
}
Write-Output ("Passed {0} cleanup matcher cases, {1} evidence cases, model promotion and receipt bounds." -f
  $fixture.cases.Count, $fixture.evidence.Count)
