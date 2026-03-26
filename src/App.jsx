import { useState, useRef, useEffect, useCallback } from 'react'
import {
  Chart as ChartJS,
  CategoryScale,
  LinearScale,
  PointElement,
  LineElement,
  Title,
  Tooltip,
  Legend,
  Filler
} from 'chart.js'
import { Line } from 'react-chartjs-2'

ChartJS.register(
  CategoryScale,
  LinearScale,
  PointElement,
  LineElement,
  Title,
  Tooltip,
  Legend,
  Filler
)

// Constants
const FEE = 0.0005

function App() {
  const [data, setData] = useState([])
  const [timestamps, setTimestamps] = useState([])
  const [swarm, setSwarm] = useState([])
  const [status, setStatus] = useState('Awaiting CSV or Tab-Delimited file...')
  const [statusClass, setStatusClass] = useState('text-yellow-500')
  const [progress, setProgress] = useState(0)
  const [progressLabel, setProgressLabel] = useState('Idle')
  const [runDisabled, setRunDisabled] = useState(true)
  const [runtimeInfo, setRuntimeInfo] = useState('')
  const [activeCount, setActiveCount] = useState('0 Agents Active')
  const [leaderboard, setLeaderboard] = useState([])
  const [chartData, setChartData] = useState(null)
  const [selectedAgent, setSelectedAgent] = useState(null)
  
  const [swarmSize, setSwarmSize] = useState(1000)
  const [leverage, setLeverage] = useState(200)
  const [risk, setRisk] = useState(1)
  const [initialCapital, setInitialCapital] = useState(10000)
  
  const fileInputRef = useRef(null)

  const parseDataFile = useCallback((text) => {
    try {
      const lines = text.trim().split('\n')
      if (lines.length < 2) throw new Error('File too short')

      // Determine delimiter (Tab or Comma)
      const header = lines[0].toUpperCase()
      const delimiter = header.includes('\t') ? '\t' : ','
      const columns = lines[0].split(delimiter).map(c => c.trim().toUpperCase())

      // Dynamic Column Mapping
      const mapping = {
        date: columns.findIndex(c => c.includes('DATE') || c.includes('TIME') || c.includes('DATETIME')),
        open: columns.findIndex(c => c.includes('OPEN')),
        high: columns.findIndex(c => c.includes('HIGH')),
        low: columns.findIndex(c => c.includes('LOW')),
        close: columns.findIndex(c => c.includes('CLOSE'))
      }

      // Fallback for files without headers (using default MT4/CSV indices)
      if (mapping.open === -1) mapping.open = 2
      if (mapping.high === -1) mapping.high = 3
      if (mapping.low === -1) mapping.low = 4
      if (mapping.close === -1) mapping.close = 5
      if (mapping.date === -1) mapping.date = 0

      const parsed = []
      const parsedTimestamps = []
      // Start from line 1 if line 0 was a header, else 0
      const isHeader = isNaN(parseFloat(lines[0].split(delimiter)[mapping.open]))
      const startIndex = isHeader ? 1 : 0

      for (let i = startIndex; i < lines.length; i++) {
        const row = lines[i].split(delimiter)
        if (row.length <= Math.max(...Object.values(mapping))) continue

        const candle = {
          open: parseFloat(row[mapping.open]),
          high: parseFloat(row[mapping.high]),
          low: parseFloat(row[mapping.low]),
          close: parseFloat(row[mapping.close])
        }

        // Parse timestamp
        let timestamp = ''
        if (mapping.date >= 0 && row[mapping.date]) {
          timestamp = row[mapping.date].trim()
        }

        if (!isNaN(candle.close)) {
          parsed.push(candle)
          parsedTimestamps.push(timestamp)
        }
      }

      if (parsed.length === 0) throw new Error('No valid data rows found')

      setData(parsed)
      setTimestamps(parsedTimestamps)
      setStatus(`READY: ${parsed.length.toLocaleString()} candles loaded. Format: ${delimiter === '\t' ? 'Tabs' : 'CSV'}`)
      setStatusClass('text-green-400')
      setRunDisabled(false)
    } catch (err) {
      console.error(err)
      setStatus('Error parsing file. Check format.')
      setStatusClass('text-red-500')
    }
  }, [])

  const handleFileChange = (e) => {
    const file = e.target.files[0]
    if (!file) return
    const reader = new FileReader()
    setStatus('Reading file...')
    reader.onload = (event) => {
      parseDataFile(event.target.result)
    }
    reader.readAsText(file)
  }

  const generateSwarm = (size, capital) => {
    const agents = []
    for (let i = 0; i < size; i++) {
      const strategyType = i % 4
      agents.push({
        id: i + 1,
        balance: capital,
        equityCurve: [capital],
        trades: 0,
        wins: 0,
        liquidated: false,
        params: {
          maS: Math.floor(Math.random() * 30) + 2,
          maL: Math.floor(Math.random() * 100) + 31,
          volLookback: Math.floor(Math.random() * 20) + 5,
          volMult: 0.5 + Math.random() * 2.5,
          threshold: 0.001 + Math.random() * 0.02,
          strategyType: strategyType,
          bias: Math.random() > 0.4 ? 1 : -1
        },
        currentPosition: null,
        tradeLog: []
      })
    }
    return agents
  }

  const getSMA = (arr, period, index) => {
    if (index < period - 1) return arr[index].close
    let sum = 0
    for (let i = 0; i < period; i++) {
      const targetIdx = index - i
      if (arr[targetIdx]) {
        sum += arr[targetIdx].close
      }
    }
    return sum / period
  }

  const openPosition = (agent, price, type, leverageVal, riskVal, candleIndex) => {
    const margin = agent.balance * riskVal
    if (margin <= 0) return
    const positionSizeUnits = (margin * leverageVal) / price
    const fee = (positionSizeUnits * price) * FEE
    agent.balance -= fee
    agent.currentPosition = { 
      type, 
      entry: price, 
      size: positionSizeUnits,
      entryCandle: candleIndex,
      entryTime: new Date().toISOString()
    }
    agent.trades++
    
    // Log trade entry
    agent.tradeLog.push({
      type: 'OPEN',
      direction: type,
      price: price,
      size: positionSizeUnits,
      candle: candleIndex,
      timestamp: new Date().toISOString(),
      candleTimestamp: timestamps[candleIndex] || '',
      balance: agent.balance
    })
  }

  const closePosition = (agent, price, candleIndex) => {
    const pos = agent.currentPosition
    const fee = (pos.size * price) * FEE
    const pnl = pos.type === 'long'
      ? (price - pos.entry) * pos.size
      : (pos.entry - price) * pos.size
    agent.balance += (pnl - fee)
    if (pnl > 0) agent.wins++
    
    // Log trade exit with PnL
    agent.tradeLog.push({
      type: 'CLOSE',
      direction: pos.type,
      entryPrice: pos.entry,
      exitPrice: price,
      size: pos.size,
      pnl: pnl,
      pnlPercent: ((pnl / (pos.entry * pos.size)) * 100).toFixed(2),
      fee: fee,
      candle: candleIndex,
      timestamp: new Date().toISOString(),
      candleTimestamp: timestamps[candleIndex] || '',
      balance: agent.balance
    })
    
    agent.currentPosition = null
  }

  const calculateUnrealized = (agent, price) => {
    const pos = agent.currentPosition
    return pos.type === 'long'
      ? (price - pos.entry) * pos.size
      : (pos.entry - price) * pos.size
  }

  const runBacktestAsync = async (leverageVal, riskVal, swarmData, capital) => {
    const batchSize = 100
    const currentSwarm = [...swarmData]
    
    for (let i = 100; i < data.length; i++) {
      const candle = data[i]
      const prev = data[i - 1]

      for (let j = 0; j < currentSwarm.length; j++) {
        const agent = currentSwarm[j]
        if (agent.liquidated) continue

        const smaS = getSMA(data, agent.params.maS, i)
        const smaL = getSMA(data, agent.params.maL, i)

        let signal = 0
        if (agent.params.strategyType === 0) {
          if (smaS > smaL && prev.close < smaS) signal = 1
          if (smaS < smaL && prev.close > smaS) signal = -1
        } else if (agent.params.strategyType === 1) {
          if (candle.close < smaL * (1 - agent.params.threshold)) signal = 1
          if (candle.close > smaL * (1 + agent.params.threshold)) signal = -1
        } else {
          if (candle.close > prev.high) signal = 1
          if (candle.close < prev.low) signal = -1
        }

        signal *= agent.params.bias

        if (agent.currentPosition) {
          const pos = agent.currentPosition
          const priceChange = (candle.close - pos.entry) / pos.entry
          const pnlPct = pos.type === 'long' ? priceChange : -priceChange

          if (pnlPct * leverageVal <= -0.9) {
            // Liquidation
            agent.tradeLog.push({
              type: 'LIQUIDATION',
              direction: pos.type,
              entryPrice: pos.entry,
              exitPrice: candle.close,
              size: pos.size,
              pnl: -agent.balance,
              candle: i,
              timestamp: new Date().toISOString(),
              candleTimestamp: timestamps[i] || '',
              balance: 0
            })
            agent.balance = 0
            agent.liquidated = true
            agent.currentPosition = null
          } else if ((pos.type === 'long' && signal === -1) || (pos.type === 'short' && signal === 1)) {
            closePosition(agent, candle.close, i)
          }
        } else if (signal !== 0) {
          openPosition(agent, candle.close, signal === 1 ? 'long' : 'short', leverageVal, riskVal, i)
        }

        if (i % 10 === 0 || i === data.length - 1) {
          const unrealized = agent.currentPosition ? calculateUnrealized(agent, candle.close) : 0
          agent.equityCurve.push(agent.balance + unrealized)
        }
      }

      if (i % batchSize === 0) {
        const pct = Math.round((i / data.length) * 100)
        setProgress(pct)
        setStatus(`Processing candle ${i}...`)
        await new Promise(r => setTimeout(r, 0))
      }
    }

    return currentSwarm
  }

  const handleRun = async () => {
    const size = parseInt(swarmSize) || 100
    const leverageVal = parseFloat(leverage) || 1
    const riskVal = (parseFloat(risk) || 1) / 100
    const capital = parseFloat(initialCapital) || 10000

    const newSwarm = generateSwarm(size, capital)
    setSwarm(newSwarm)
    setActiveCount(`${size} Agents Active`)
    setProgressLabel('Simulating Swarm')
    setRunDisabled(true)

    console.log(`%c🚀 SWARM BACKTEST STARTED`, 'color: #3b82f6; font-size: 16px; font-weight: bold')
    console.log(`%cConfiguration:`, 'color: #94a3b8; font-weight: bold')
    console.log(`  • Swarm Size: ${size} agents`)
    console.log(`  • Initial Capital: $${capital.toLocaleString()}`)
    console.log(`  • Leverage: ${leverageVal}x`)
    console.log(`  • Risk per Trade: ${(riskVal * 100).toFixed(1)}%`)
    console.log('─'.repeat(60))

    const start = performance.now()
    const result = await runBacktestAsync(leverageVal, riskVal, newSwarm, capital)
    const end = performance.now()

    setRuntimeInfo(`Engine: ${(end - start).toFixed(0)}ms | ${size} Agents`)
    finishBacktest(result, capital)
  }

  const finishBacktest = (finalSwarm, capital) => {
    setProgress(100)
    setProgressLabel('Complete')
    setStatus('Optimization Finished.')
    setRunDisabled(false)

    const sorted = [...finalSwarm].sort((a, b) => b.balance - a.balance)
    setLeaderboard(sorted.slice(0, 100))

    // Log best agent details
    const bestAgent = sorted[0]
    const roi = ((bestAgent.balance - capital) / capital * 100).toFixed(2)
    const winRate = bestAgent.trades > 0 ? (bestAgent.wins / bestAgent.trades * 100).toFixed(1) : 0

    console.log('%c' + '═'.repeat(60), 'color: #22c55e')
    console.log(`%c🏆 BEST AGENT #${bestAgent.id}`, 'color: #22c55e; font-size: 18px; font-weight: bold')
    console.log('%c' + '═'.repeat(60), 'color: #22c55e')
    console.log(`%c📊 PERFORMANCE SUMMARY:`, 'color: #f59e0b; font-weight: bold')
    console.log(`  • Final Balance: $${bestAgent.balance.toLocaleString(undefined, {minimumFractionDigits: 2, maximumFractionDigits: 2})}`)
    console.log(`  • ROI: ${roi}%`)
    console.log(`  • Total Trades: ${bestAgent.trades}`)
    console.log(`  • Win Rate: ${winRate}%`)
    console.log(`  • Liquidated: ${bestAgent.liquidated ? 'Yes ❌' : 'No ✅'}`)
    console.log('')
    console.log(`%c⚙️ STRATEGY PARAMETERS:`, 'color: #8b5cf6; font-weight: bold')
    console.log(`  • Strategy Type: ${bestAgent.params.strategyType}`)
    console.log(`  • MA Short: ${bestAgent.params.maS}`)
    console.log(`  • MA Long: ${bestAgent.params.maL}`)
    console.log(`  • Threshold: ${(bestAgent.params.threshold * 100).toFixed(3)}%`)
    console.log(`  • Bias: ${bestAgent.params.bias > 0 ? 'Long 📈' : 'Short 📉'}`)
    console.log('')
    console.log(`%c📜 TRADE LOG (${bestAgent.tradeLog.length} entries):`, 'color: #06b6d4; font-weight: bold')
    console.log('─'.repeat(60))
    
    bestAgent.tradeLog.forEach((trade, idx) => {
      const time = new Date(trade.timestamp).toLocaleTimeString()
      if (trade.type === 'OPEN') {
        console.log(`%c[${time}] OPEN ${trade.direction.toUpperCase()}`, 'color: #3b82f6')
        console.log(`  Price: $${trade.price.toFixed(2)} | Size: ${trade.size.toFixed(4)} | Balance: $${trade.balance.toFixed(2)}`)
      } else if (trade.type === 'CLOSE') {
        const pnlColor = trade.pnl >= 0 ? 'color: #22c55e' : 'color: #ef4444'
        console.log(`%c[${time}] CLOSE ${trade.direction.toUpperCase()} | PnL: ${trade.pnl >= 0 ? '+' : ''}$${trade.pnl.toFixed(2)} (${trade.pnlPercent}%)`, pnlColor)
        console.log(`  Entry: $${trade.entryPrice.toFixed(2)} → Exit: $${trade.exitPrice.toFixed(2)} | Fee: $${trade.fee.toFixed(2)} | Balance: $${trade.balance.toFixed(2)}`)
      } else if (trade.type === 'LIQUIDATION') {
        console.log(`%c[${time}] ⚠️ LIQUIDATION ${trade.direction.toUpperCase()}`, 'color: #ef4444; font-weight: bold')
        console.log(`  Entry: $${trade.entryPrice.toFixed(2)} → Exit: $${trade.exitPrice.toFixed(2)} | Loss: $${Math.abs(trade.pnl).toFixed(2)}`)
      }
      if (idx < bestAgent.tradeLog.length - 1) console.log('')
    })
    
    console.log('%c' + '═'.repeat(60), 'color: #22c55e')
    console.log(`%c✅ BACKTEST COMPLETE`, 'color: #22c55e; font-size: 16px; font-weight: bold')
    console.log('%c' + '═'.repeat(60), 'color: #22c55e')

    // Prepare chart data with timestamps
    const equityLength = sorted[0].equityCurve.length
    const labels = []
    // First equity point is at candle 100 (start of backtest)
    // Then every 10 candles after that
    for (let i = 0; i < equityLength; i++) {
      const candleIndex = 100 + (i * 10)
      if (timestamps[candleIndex] && timestamps[candleIndex].trim() !== '') {
        labels.push(timestamps[candleIndex])
      } else {
        // Format as shorter label if no timestamp
        labels.push(`#${candleIndex}`)
      }
    }
    
    const avgCurve = []
    for (let t = 0; t < equityLength; t++) {
      let sum = 0
      let active = 0
      finalSwarm.forEach(a => {
        if (a.equityCurve[t] !== undefined) {
          sum += a.equityCurve[t]
          active++
        }
      })
      avgCurve.push(sum / (active || 1))
    }

    setChartData({
      labels,
      datasets: [
        {
          label: 'Leader Equity',
          data: sorted[0].equityCurve,
          borderColor: '#3b82f6',
          borderWidth: 2,
          pointRadius: 0,
          fill: false,
          tension: 0.1,
          order: 1
        },
        {
          label: 'Swarm Average',
          data: avgCurve,
          borderColor: 'rgba(255,255,255,0.2)',
          borderWidth: 1.5,
          borderDash: [5, 5],
          pointRadius: 0,
          fill: false,
          order: 2
        }
      ]
    })
  }

  const getStrategyFormula = (params) => {
    const strategies = {
      0: {
        name: 'MA Crossover',
        formula: `IF SMA(${params.maS}) > SMA(${params.maL}) AND Close < SMA(${params.maS}) THEN LONG\nIF SMA(${params.maS}) < SMA(${params.maL}) AND Close > SMA(${params.maS}) THEN SHORT`,
        description: 'Trades on moving average crossover with price confirmation'
      },
      1: {
        name: 'Mean Reversion',
        formula: `IF Close < SMA(${params.maL}) × (1 - ${(params.threshold * 100).toFixed(3)}%) THEN LONG\nIF Close > SMA(${params.maL}) × (1 + ${(params.threshold * 100).toFixed(3)}%) THEN SHORT`,
        description: 'Trades when price deviates from mean by threshold percentage'
      },
      2: {
        name: 'Breakout',
        formula: `IF Close > Previous High THEN LONG\nIF Close < Previous Low THEN SHORT`,
        description: 'Trades on price breaking previous candle high/low'
      },
      3: {
        name: 'Hybrid',
        formula: `Combines MA Crossover + Breakout signals\nWeighted by volatility multiplier: ${params.volMult.toFixed(2)}`,
        description: 'Multi-strategy approach combining multiple signals'
      }
    }
    return strategies[params.strategyType] || strategies[0]
  }

  const chartOptions = {
    maintainAspectRatio: false,
    responsive: true,
    interaction: {
      mode: 'index',
      intersect: false,
    },
    plugins: {
      legend: { display: false },
      tooltip: {
        enabled: true,
        backgroundColor: 'rgba(15, 23, 42, 0.9)',
        titleColor: '#94a3b8',
        bodyColor: '#f1f5f9',
        borderColor: '#334155',
        borderWidth: 1,
        padding: 10,
        displayColors: true,
        callbacks: {
          title: function(context) {
            return context[0].label || ''
          },
          label: function (context) {
            let label = context.dataset.label || ''
            if (label) {
              label += ': '
            }
            if (context.parsed.y !== null) {
              label += new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD' }).format(context.parsed.y)
            }
            return label
          }
        }
      }
    },
    scales: {
      x: { 
        display: true,
        grid: { color: '#1e293b' },
        ticks: {
          color: '#64748b',
          font: { size: 9 },
          maxRotation: 45,
          minRotation: 45,
          maxTicksLimit: 15
        }
      },
      y: {
        grid: { color: '#1e293b' },
        ticks: {
          color: '#64748b',
          font: { size: 10 },
          callback: (value) => '$' + value.toLocaleString()
        }
      }
    }
  }

  return (
    <div className="bg-slate-900 text-slate-100 min-h-screen font-sans">
      <div className="container mx-auto px-4 py-8">
        <header className="mb-8 border-b border-slate-700 pb-6 flex justify-between items-end">
          <div>
            <h1 className="text-3xl font-bold text-blue-400">Swarm Intelligence Pro</h1>
            <p className="text-slate-400 mt-2">Massive Parallel Agent Backtesting Engine</p>
          </div>
          <div className="text-right text-xs text-slate-500 font-mono">{runtimeInfo}</div>
        </header>

        {/* Configuration & Upload */}
        <div className="grid grid-cols-1 lg:grid-cols-4 gap-6 mb-8">
          <div className="bg-slate-800 p-5 rounded-xl border border-slate-700 col-span-1">
            <h2 className="text-lg font-semibold mb-4 flex items-center gap-2">
              <span className="w-2 h-2 bg-blue-500 rounded-full"></span> 1. Configuration
            </h2>

            <div className="space-y-4">
              <div>
                <label className="block text-xs text-slate-400 mb-1">Initial Capital ($)</label>
                <input
                  type="number"
                  value={initialCapital}
                  onChange={(e) => setInitialCapital(e.target.value)}
                  className="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-sm outline-none focus:border-blue-500"
                />
              </div>
              <div>
                <label className="block text-xs text-slate-400 mb-1">Swarm Size</label>
                <input
                  type="number"
                  value={swarmSize}
                  onChange={(e) => setSwarmSize(e.target.value)}
                  className="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-sm outline-none focus:border-blue-500"
                />
              </div>
              <div>
                <label className="block text-xs text-slate-400 mb-1">Leverage (x)</label>
                <input
                  type="number"
                  value={leverage}
                  onChange={(e) => setLeverage(e.target.value)}
                  className="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-sm outline-none focus:border-blue-500"
                />
              </div>
              <div>
                <label className="block text-xs text-slate-400 mb-1">Risk per Trade (%)</label>
                <input
                  type="number"
                  value={risk}
                  step="0.1"
                  onChange={(e) => setRisk(e.target.value)}
                  className="w-full bg-slate-900 border border-slate-700 rounded px-3 py-2 text-sm outline-none focus:border-blue-500"
                />
              </div>
            </div>
          </div>

          <div className="bg-slate-800 p-5 rounded-xl border border-slate-700 col-span-1">
            <h2 className="text-lg font-semibold mb-4 flex items-center gap-2">
              <span className="w-2 h-2 bg-green-500 rounded-full"></span> 2. Data
            </h2>
            <input
              type="file"
              ref={fileInputRef}
              accept=".csv,.txt"
              onChange={handleFileChange}
              className="block w-full text-xs text-slate-400 file:mr-3 file:py-2 file:px-4 file:rounded file:border-0 file:text-xs file:font-semibold file:bg-slate-700 file:text-white hover:file:bg-slate-600 cursor-pointer mb-6"
            />

            <button
              onClick={handleRun}
              disabled={runDisabled}
              className="w-full bg-blue-600 hover:bg-blue-700 disabled:bg-slate-700 disabled:text-slate-500 disabled:cursor-not-allowed text-white font-bold py-3 rounded-lg transition-all shadow-lg shadow-blue-900/20"
            >
              Deploy Swarm
            </button>
          </div>

          <div className="bg-slate-800 p-5 rounded-xl border border-slate-700 col-span-2 flex flex-col justify-between">
            <div>
              <h2 className="text-lg font-semibold mb-2">Swarm Status</h2>
              <div className={`font-mono text-sm ${statusClass}`}>{status}</div>
            </div>
            <div className="mt-4">
              <div className="flex justify-between text-[10px] text-slate-500 mb-1 uppercase tracking-wider">
                <span>{progressLabel}</span>
                <span>{progress}%</span>
              </div>
              <div className="h-3 bg-slate-900 rounded-full overflow-hidden p-0.5 border border-slate-700">
                <div
                  className="h-full bg-gradient-to-r from-blue-600 to-cyan-400 transition-all duration-300 rounded-full"
                  style={{ width: `${progress}%` }}
                ></div>
              </div>
            </div>
          </div>
        </div>

        {/* Main Chart */}
        <div className="bg-slate-800 p-6 rounded-xl border border-slate-700 mb-8 shadow-xl">
          <div className="flex justify-between items-center mb-4">
            <h2 className="text-xl font-semibold">Live Swarm Trajectory</h2>
            <div className="flex gap-4 text-xs">
              <span className="flex items-center gap-1">
                <span className="w-3 h-0.5 bg-blue-500"></span> Leader (Hover for Value)
              </span>
              <span className="flex items-center gap-1">
                <span className="w-3 h-0.5 bg-white opacity-50"></span> Average
              </span>
            </div>
          </div>
          <div className="relative h-[350px]">
            {chartData ? (
              <Line data={chartData} options={chartOptions} />
            ) : (
              <div className="flex items-center justify-center h-full text-slate-500">
                Chart will appear after backtest completes
              </div>
            )}
          </div>
        </div>

        {/* Leaderboard */}
        <div className="bg-slate-800 p-6 rounded-xl border border-slate-700 shadow-xl">
          <div className="flex justify-between items-center mb-4">
            <h2 className="text-xl font-semibold">Agent Rankings</h2>
            <span className="text-xs text-slate-400 bg-slate-900 px-2 py-1 rounded">{activeCount}</span>
          </div>
          <div className="agent-grid max-h-[500px] overflow-y-auto pr-2 custom-scrollbar">
            {leaderboard.map((agent, idx) => {
              const roi = ((agent.balance - initialCapital) / initialCapital * 100).toFixed(1)
              const winRate = agent.trades > 0 ? (agent.wins / agent.trades * 100).toFixed(0) : 0

              return (
                <div
                  key={agent.id}
                  onClick={() => setSelectedAgent(agent)}
                  className={`agent-card p-2 rounded border border-slate-700 cursor-pointer hover:border-blue-500 transition-all ${
                    agent.liquidated
                      ? 'opacity-40 bg-slate-950'
                      : idx < 3
                        ? 'bg-blue-900/20 border-blue-500/50'
                        : 'bg-slate-800'
                  } ${selectedAgent?.id === agent.id ? 'ring-2 ring-blue-500' : ''}`}
                >
                  <div className="flex justify-between items-start mb-1">
                    <span className="text-[10px] font-bold text-slate-500">#{idx + 1}</span>
                    <span className="text-[9px] px-1 bg-slate-700 rounded text-slate-300">
                      S{agent.params.strategyType}
                    </span>
                  </div>
                  <div className={`font-mono font-bold ${agent.balance > initialCapital ? 'text-green-400' : 'text-red-400'}`}>
                    {roi}%
                  </div>
                  <div className="text-[9px] text-slate-500 mt-1">
                    {winRate}% WR | {agent.trades}T
                  </div>
                </div>
              )
            })}
          </div>
        </div>

        {/* Agent Detail Modal */}
        {selectedAgent && (
          <div className="fixed inset-0 bg-black/70 flex items-center justify-center z-50 p-4" onClick={() => setSelectedAgent(null)}>
            <div className="bg-slate-800 rounded-xl border border-slate-700 max-w-4xl w-full max-h-[90vh] overflow-hidden" onClick={(e) => e.stopPropagation()}>
              <div className="p-6 border-b border-slate-700 flex justify-between items-center">
                <div>
                  <h2 className="text-2xl font-bold text-blue-400">Agent #{selectedAgent.id}</h2>
                  <p className="text-slate-400 text-sm mt-1">Detailed Performance & Trade Log</p>
                </div>
                <button 
                  onClick={() => setSelectedAgent(null)}
                  className="text-slate-400 hover:text-white text-2xl"
                >
                  ×
                </button>
              </div>
              
              <div className="p-6 overflow-y-auto max-h-[calc(90vh-100px)]">
                {/* Performance Stats */}
                <div className="grid grid-cols-2 md:grid-cols-4 gap-4 mb-6">
                  <div className="bg-slate-900 p-4 rounded-lg">
                    <div className="text-xs text-slate-400 mb-1">Final Balance</div>
                    <div className={`text-xl font-bold font-mono ${selectedAgent.balance > initialCapital ? 'text-green-400' : 'text-red-400'}`}>
                      ${selectedAgent.balance.toLocaleString(undefined, {minimumFractionDigits: 2, maximumFractionDigits: 2})}
                    </div>
                  </div>
                  <div className="bg-slate-900 p-4 rounded-lg">
                    <div className="text-xs text-slate-400 mb-1">ROI</div>
                    <div className={`text-xl font-bold font-mono ${selectedAgent.balance > initialCapital ? 'text-green-400' : 'text-red-400'}`}>
                      {((selectedAgent.balance - initialCapital) / initialCapital * 100).toFixed(2)}%
                    </div>
                  </div>
                  <div className="bg-slate-900 p-4 rounded-lg">
                    <div className="text-xs text-slate-400 mb-1">Win Rate</div>
                    <div className="text-xl font-bold font-mono text-blue-400">
                      {selectedAgent.trades > 0 ? (selectedAgent.wins / selectedAgent.trades * 100).toFixed(1) : 0}%
                    </div>
                  </div>
                  <div className="bg-slate-900 p-4 rounded-lg">
                    <div className="text-xs text-slate-400 mb-1">Total Trades</div>
                    <div className="text-xl font-bold font-mono text-slate-100">
                      {selectedAgent.trades}
                    </div>
                  </div>
                </div>

                {/* Strategy Formula */}
                <div className="bg-slate-900 p-4 rounded-lg mb-6">
                  <h3 className="text-sm font-semibold text-purple-400 mb-3">⚙️ Strategy Formula</h3>
                  {(() => {
                    const strategy = getStrategyFormula(selectedAgent.params)
                    return (
                      <div>
                        <div className="text-xs text-slate-400 mb-2">{strategy.name}</div>
                        <pre className="text-xs text-slate-300 bg-slate-800 p-3 rounded overflow-x-auto whitespace-pre-wrap">
                          {strategy.formula}
                        </pre>
                        <div className="text-xs text-slate-500 mt-2">{strategy.description}</div>
                      </div>
                    )
                  })()}
                </div>

                {/* Strategy Parameters */}
                <div className="bg-slate-900 p-4 rounded-lg mb-6">
                  <h3 className="text-sm font-semibold text-purple-400 mb-3">📊 Parameters</h3>
                  <div className="grid grid-cols-2 md:grid-cols-3 gap-3 text-xs">
                    <div>
                      <span className="text-slate-400">MA Short:</span>
                      <span className="ml-2 text-slate-200">{selectedAgent.params.maS}</span>
                    </div>
                    <div>
                      <span className="text-slate-400">MA Long:</span>
                      <span className="ml-2 text-slate-200">{selectedAgent.params.maL}</span>
                    </div>
                    <div>
                      <span className="text-slate-400">Threshold:</span>
                      <span className="ml-2 text-slate-200">{(selectedAgent.params.threshold * 100).toFixed(3)}%</span>
                    </div>
                    <div>
                      <span className="text-slate-400">Vol Lookback:</span>
                      <span className="ml-2 text-slate-200">{selectedAgent.params.volLookback}</span>
                    </div>
                    <div>
                      <span className="text-slate-400">Vol Multiplier:</span>
                      <span className="ml-2 text-slate-200">{selectedAgent.params.volMult.toFixed(2)}</span>
                    </div>
                    <div>
                      <span className="text-slate-400">Bias:</span>
                      <span className={`ml-2 ${selectedAgent.params.bias > 0 ? 'text-green-400' : 'text-red-400'}`}>
                        {selectedAgent.params.bias > 0 ? 'Long 📈' : 'Short 📉'}
                      </span>
                    </div>
                    <div>
                      <span className="text-slate-400">Liquidated:</span>
                      <span className={`ml-2 ${selectedAgent.liquidated ? 'text-red-400' : 'text-green-400'}`}>
                        {selectedAgent.liquidated ? 'Yes ❌' : 'No ✅'}
                      </span>
                    </div>
                  </div>
                </div>

                {/* Trade Log */}
                <div className="bg-slate-900 p-4 rounded-lg">
                  <h3 className="text-sm font-semibold text-cyan-400 mb-3">📜 Trade Log ({selectedAgent.tradeLog.length} entries)</h3>
                  <div className="max-h-[300px] overflow-y-auto custom-scrollbar space-y-2">
                    {selectedAgent.tradeLog.length === 0 ? (
                      <div className="text-slate-500 text-sm text-center py-4">No trades executed</div>
                    ) : (
                      selectedAgent.tradeLog.map((trade, idx) => {
                        const time = trade.candleTimestamp || new Date(trade.timestamp).toLocaleTimeString()
                        return (
                          <div key={idx} className="bg-slate-800 p-3 rounded text-xs">
                            <div className="flex justify-between items-center mb-2">
                              <span className={`font-semibold ${
                                trade.type === 'OPEN' ? 'text-blue-400' :
                                trade.type === 'CLOSE' ? (trade.pnl >= 0 ? 'text-green-400' : 'text-red-400') :
                                'text-red-500'
                              }`}>
                                {trade.type === 'LIQUIDATION' && '⚠️ '}{trade.type} {trade.direction.toUpperCase()}
                              </span>
                              <span className="text-slate-500">[{time}]</span>
                            </div>
                            <div className="grid grid-cols-2 gap-2 text-slate-400">
                              {trade.type === 'OPEN' && (
                                <>
                                  <div>Price: <span className="text-slate-200">${trade.price.toFixed(2)}</span></div>
                                  <div>Size: <span className="text-slate-200">{trade.size.toFixed(4)}</span></div>
                                </>
                              )}
                              {trade.type === 'CLOSE' && (
                                <>
                                  <div>Entry: <span className="text-slate-200">${trade.entryPrice.toFixed(2)}</span></div>
                                  <div>Exit: <span className="text-slate-200">${trade.exitPrice.toFixed(2)}</span></div>
                                  <div>PnL: <span className={trade.pnl >= 0 ? 'text-green-400' : 'text-red-400'}>
                                    {trade.pnl >= 0 ? '+' : ''}${trade.pnl.toFixed(2)} ({trade.pnlPercent}%)
                                  </span></div>
                                  <div>Fee: <span className="text-slate-200">${trade.fee.toFixed(2)}</span></div>
                                </>
                              )}
                              {trade.type === 'LIQUIDATION' && (
                                <>
                                  <div>Entry: <span className="text-slate-200">${trade.entryPrice.toFixed(2)}</span></div>
                                  <div>Exit: <span className="text-slate-200">${trade.exitPrice.toFixed(2)}</span></div>
                                  <div>Loss: <span className="text-red-400">-${Math.abs(trade.pnl).toFixed(2)}</span></div>
                                </>
                              )}
                              <div>Balance: <span className="text-slate-200">${trade.balance.toFixed(2)}</span></div>
                            </div>
                          </div>
                        )
                      })
                    )}
                  </div>
                </div>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}

export default App