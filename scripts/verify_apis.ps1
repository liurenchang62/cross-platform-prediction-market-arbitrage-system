# =============================================================================
# 目的（只做这三件事）:
#   1) 确认用的 URL 与 src/clients.rs 一致，且返回 HTTP 200 + 预期 JSON 形状
#   2) 若 bids/asks 或 yes_dollars/no_dollars 行数为 0 -> 多半是市场无挂单，不是「接口选错」
#   3) 若没有 bids/asks 字段或没有 orderbook 结构 -> 才怀疑接口变更或解析要对齐文档
#
# 为何不用 curl: Windows 上 curl.exe 常不走系统代理；浏览器能开、脚本超时多因此。
# 本脚本用 Invoke-RestMethod，一般与 IE/系统代理策略一致（与你「网页能开」更接近）。
#
#   powershell -ExecutionPolicy Bypass -File .\scripts\verify_apis.ps1
#   powershell -ExecutionPolicy Bypass -File .\scripts\verify_apis.ps1 -KalshiBase 'https://demo-api.kalshi.co/trade-api/v2'
#   powershell -ExecutionPolicy Bypass -File .\scripts\verify_apis.ps1 -ShowFullBook
# =============================================================================

param(
    [string]$KalshiBase = 'https://api.elections.kalshi.com/trade-api/v2',
    [switch]$ShowFullBook
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$outDir = Join-Path $repoRoot 'tmp_api_probe'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$KalshiBase = $KalshiBase.TrimEnd('/')

function Save-Json {
    param($Obj, [string]$Path)
    ($Obj | ConvertTo-Json -Depth 25 -Compress:$false) | Set-Content -Path $Path -Encoding UTF8
}

function Get-Json {
    param([string]$Uri, [hashtable]$Headers = @{})
    # Invoke-RestMethod: no UseBasicParsing (that is Invoke-WebRequest only in Windows PowerShell 5.1)
    $p = @{ Uri = $Uri; Method = 'Get'; TimeoutSec = 60 }
    if ($Headers.Count -gt 0) { $p.Headers = $Headers }
    try {
        return Invoke-RestMethod @p
    }
    catch {
        $msg = $_.Exception.Message
        if ($_.ErrorDetails -and $_.ErrorDetails.Message) { $msg = $_.ErrorDetails.Message }
        throw "GET failed: $Uri`n$msg"
    }
}

function Get-JsonObjectPropertyNames {
    param($Obj)
    if ($null -eq $Obj) { return @() }
    @($Obj.PSObject.Properties.Name)
}

function Resolve-KalshiOrderbookNode {
    param($Root)
    if ($null -eq $Root) { return $null }
    if ((Get-JsonObjectPropertyNames $Root) -contains 'orderbook_fp') { return $Root.orderbook_fp }
    $ob = $Root.orderbook
    if ($null -eq $ob) { return $null }
    if ((Get-JsonObjectPropertyNames $ob) -contains 'orderbook_fp') { return $ob.orderbook_fp }
    return $ob
}

Write-Host ''
Write-Host '### GOAL: validate endpoints + tell empty-book vs broken JSON ###' -ForegroundColor DarkGray
Write-Host ('Polymarket book: https://clob.polymarket.com/book?token_id=<from Gamma>' ) -ForegroundColor DarkGray
Write-Host ('Kalshi book:      ' + $KalshiBase + '/markets/<ticker>/orderbook') -ForegroundColor DarkGray
Write-Host ''

# --- 1 Gamma ---
Write-Host 'Step 1) Gamma markets' -ForegroundColor Cyan
$gammaUrl = 'https://gamma-api.polymarket.com/markets?limit=1&closed=false'
Write-Host ('URL: ' + $gammaUrl)
$gamma = @(Get-Json $gammaUrl)
Save-Json $gamma (Join-Path $outDir 'gamma_markets.json')
if ($gamma.Count -lt 1) { throw 'Gamma: empty' }
$m0 = $gamma[0]
Write-Host ($m0 | ConvertTo-Json -Depth 6 -Compress)

$rawIds = $m0.clobTokenIds
$tid = $null
if ($rawIds -is [string]) {
    $arr = $rawIds | ConvertFrom-Json
    $tid = @($arr)[0]
}
elseif ($rawIds -is [System.Array]) {
    $tid = $rawIds[0]
}
if (-not $tid) { throw 'Could not parse clobTokenIds' }

Write-Host ('[+] Gamma HTTP OK. YES token_id=' + $tid) -ForegroundColor Green

# --- 2 CLOB ---
Write-Host ''
Write-Host 'Step 2) Polymarket CLOB book' -ForegroundColor Cyan
$encTid = [uri]::EscapeDataString([string]$tid)
$bookUrl = 'https://clob.polymarket.com/book?token_id=' + $encTid
Write-Host ('URL: ' + $bookUrl)
$book = Get-Json $bookUrl
Save-Json $book (Join-Path $outDir 'clob_book.json')
if ($ShowFullBook) { Write-Host ($book | ConvertTo-Json -Depth 10) }

$bids = @($book.bids)
$asks = @($book.asks)
$hasBids = (Get-JsonObjectPropertyNames $book) -contains 'bids'
$hasAsks = (Get-JsonObjectPropertyNames $book) -contains 'asks'
if (-not $hasBids -or -not $hasAsks) {
    Write-Host '[!] Response missing bids or asks keys — compare with Rust parse_polymarket_buy_asks.' -ForegroundColor Red
}
Write-Host ('[+] CLOB bid_rows=' + $bids.Count + ' ask_rows=' + $asks.Count)
if ($bids.Count -gt 0) { Write-Host ('    sample bid: ' + ($bids[0] | ConvertTo-Json -Compress)) }
if ($asks.Count -gt 0) { Write-Host ('    sample ask: ' + ($asks[0] | ConvertTo-Json -Compress)) }
if ($hasAsks -and $asks.Count -eq 0) {
    Write-Host '[conclusion] API path OK; asks array empty = no sell liquidity on this token (market), not wrong URL.' -ForegroundColor Yellow
}

# --- 3 Kalshi list ---
Write-Host ''
Write-Host 'Step 3) Kalshi markets' -ForegroundColor Cyan
$kalListUrl = $KalshiBase + '/markets?limit=1&status=open'
Write-Host ('URL: ' + $kalListUrl)
$kal = Get-Json $kalListUrl @{ Accept = 'application/json' }
Save-Json $kal (Join-Path $outDir 'kalshi_markets.json')
Write-Host ($kal | ConvertTo-Json -Depth 6 -Compress)

$mk = @($kal.markets)
if ($mk.Count -lt 1) { throw 'Kalshi: no markets' }
$ticker = $mk[0].ticker
if (-not $ticker) { throw 'Kalshi: no ticker' }
Write-Host ('[+] Kalshi list OK. ticker=' + $ticker) -ForegroundColor Green

# --- 4 Kalshi orderbook ---
Write-Host ''
Write-Host 'Step 4) Kalshi orderbook' -ForegroundColor Cyan
$obUrl = $KalshiBase + '/markets/' + $ticker + '/orderbook'
Write-Host ('URL: ' + $obUrl)
$obRoot = Get-Json $obUrl @{ Accept = 'application/json' }
Save-Json $obRoot (Join-Path $outDir 'kalshi_orderbook.json')
if ($ShowFullBook) { Write-Host ($obRoot | ConvertTo-Json -Depth 12) }

$top = Get-JsonObjectPropertyNames $obRoot
Write-Host ('[+] Kalshi orderbook top keys: ' + ($top -join ', '))

$node = Resolve-KalshiOrderbookNode $obRoot
if ($null -eq $node) {
    Write-Host '[!] No orderbook / orderbook_fp node — check if API shape changed vs try_parse_kalshi_orderbook.' -ForegroundColor Red
}
else {
    $yn = @($node.yes_dollars)
    $nn = @($node.no_dollars)
    Write-Host ('[+] yes_dollars rows=' + $yn.Count + ' no_dollars rows=' + $nn.Count)
    if ($yn.Count -gt 0) { Write-Host ('    sample yes_dollars[0]: ' + ($yn[0] | ConvertTo-Json -Compress)) }
    if ($nn.Count -gt 0) { Write-Host ('    sample no_dollars[0]: ' + ($nn[0] | ConvertTo-Json -Compress)) }
    if ($yn.Count -eq 0 -and $nn.Count -eq 0) {
        Write-Host '[conclusion] Structure OK but both sides empty = no quotes (market), not wrong endpoint if HTTP succeeded.' -ForegroundColor Yellow
    }
}

Write-Host ''
Write-Host ('Artifacts: ' + $outDir) -ForegroundColor Green
Write-Host 'If all [+] and no red [!], endpoints match code; empty rows = liquidity, not API typo.' -ForegroundColor Green
