<script setup lang="ts">
import {computed, onMounted, onUnmounted, ref} from 'vue'
import {useNetworkStore} from '@/stores/network'

const store = useNetworkStore()
let timerId: number | undefined = undefined

onMounted(() => {
  // Fetch immediately on component mount
  store.netmapGet()
  // Poll for new data every 5 seconds
  timerId = window.setInterval(store.netmapGet, 5000)
})

onUnmounted(() => {
  // Clean up the timer when the component is destroyed
  if (timerId) clearInterval(timerId)
})


// Layout Constants

const viewBox = {width: 100, height: 100}
const center = {x: viewBox.width / 2, y: viewBox.height / 2}

// Radius of the polygon
const radius = 40

const nodeScale = ref(1.0)

// Base sizes
const baseNodeRadius = 4
const baseFontSize = 3

// Create computed properties that react to the slider
const nodeRadius = computed(() => baseNodeRadius * nodeScale.value)
const nodeFontSize = computed(() => baseFontSize * nodeScale.value)

// Create a style object to pass the font size to CSS
const graphStyles = computed(() => ({
  '--node-font-size': `${nodeFontSize.value}px`
}))

// Computed Positions

/**
 * Calculates the {x, y} position for each node from the store.
 */
const nodes = computed(() => {
  // Get live data from the Pinia store
  const nodeIds = Object.keys(store.nodes)
  const n = nodeIds.length
  const positions: { id: string; x: number; y: number; status: boolean }[] = []

  // Case 1: Single node at the center
  if (n === 1) {
    const id = nodeIds[0]
    positions.push({
      id: id,
      x: center.x,
      y: center.y,
      status: store.nodes[id] === 'Alive'
    })
    return positions
  }

  // Case 2+: Nodes at polygon vertices
  const startAngle = -Math.PI / 2
  const angleIncrement = (2 * Math.PI) / n

  for (let i = 0; i < n; i++) {
    const angle = startAngle + i * angleIncrement
    const x = center.x + radius * Math.cos(angle)
    const y = center.y + radius * Math.sin(angle)

    const id = nodeIds[i]
    const status = store.nodes[id] === 'Alive'

    positions.push({id, x, y, status})
  }

  return positions
})

/**
 * Generates the lines connecting adjacent nodes.
 */
const lines = computed(() => {
  const n = nodes.value.length

  // Needs at least 2 nodes to draw a line
  if (n < 2) {
    return []
  }

  const lineData: { id: string; x1: number; y1: number; x2: number; y2: number }[] = []

  for (let i = 0; i < n; i++) {
    const startNode = nodes.value[i]
    // Use the modulo operator to wrap from the last node back to the first
    const endNode = nodes.value[(i + 1) % n]

    lineData.push({
      id: `l-${startNode.id}-to-${endNode.id}`,
      x1: startNode.x,
      y1: startNode.y,
      x2: endNode.x,
      y2: endNode.y
    })
  }

  return lineData
})
</script>

<template>
  <div class="nodes-graph-wrapper">
    <div class="nodes-header">
      <div>
        <h3>Node Status</h3>
        <small v-if="!store.nodesLoading">
          Last Updated: {{ store.lastNodesUpdate }}
        </small>
        <small v-if="store.nodesLoading">Loading...</small>
      </div>
      <button @click="store.netmapGet" :disabled="store.nodesLoading">
        {{ store.nodesLoading ? 'Refreshing...' : 'Refresh' }}
      </button>
    </div>

    <div class="nodes-controls">
      <label for="nodeScaleSlider">Scale:</label>
      <input
          id="nodeScaleSlider"
          type="range"
          v-model.number="nodeScale"
          min="0.5"
          max="1.5"
          step="0.1"
      />
      <span>{{ (nodeScale * 100).toFixed(0) }}%</span>
    </div>

    <svg
        :viewBox="`0 0 ${viewBox.width} ${viewBox.height}`"
        xmlns="http://www.w3.org/2000/svg"
        class="nodes-graph"
        :style="graphStyles"
    >
      <g class="lines">
        <line
            v-for="line in lines" :key="line.id"
            :x1="line.x1"
            :y1="line.y1"
            :x2="line.x2"
            :y2="line.y2"
        />
      </g>

      <g class="nodes">
        <circle
            v-for="node in nodes"
            :key="`n-${node.id}`"
            :cx="node.x"
            :cy="node.y"
            :r="nodeRadius"
            :fill="node.status ? '#333333' : '#e63946'"
        />
      </g>

      <g class="labels">
        <text
            v-for="node in nodes"
            :key="`t-${node.id}`"
            :x="node.x"
            :y="node.y"
            dy="0.35em"
        >
          {{ node.id }}
        </text>
      </g>
    </svg>
  </div>
</template>

<style scoped>
.nodes-graph-wrapper {
  display: flex;
  flex-direction: column;
  width: 100%;
  height: 100%;
}

.nodes-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 8px 10px;
  border-bottom: 1px solid #333;
  flex-shrink: 0;
  background-color: #e8e0db;
  color: #1a1a1a;
}

.nodes-header h3 {
  margin: 0;
}

.nodes-header small {
  color: #555;
  font-size: 0.8em;
}

.nodes-header button {
  background-color: #1a1a1a;
  color: #f7f3ed;
  border: 1px solid #1a1a1a;
  padding: 4px 10px;
  border-radius: 4px;
  cursor: pointer;
  font-size: 0.9em;
}

.nodes-header button:hover {
  background-color: #444;
}

.nodes-header button:disabled {
  background-color: #aaa;
  color: #eee;
  cursor: not-allowed;
}

.nodes-controls {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 10px;
  font-size: 0.8em;
  border-bottom: 1px solid #333;
  flex-shrink: 0;
  background: #e8e0db;
  color: #1a1a1a;
}

.nodes-controls input[type="range"] {
  flex-grow: 1;
  max-width: 200px;
}

.nodes-graph {
  display: block;
  width: 100%;
  flex-grow: 1;
  min-height: 0;
  background-color: #e8e0db;
}

.lines line {
  stroke: #1a1a1a;
  stroke-width: 0.5;
}

.nodes circle {
  stroke: #1a1a1a;
  stroke-width: 0.5;
}

.labels text {
  font-size: var(--node-font-size, 3px);
  font-family: sans-serif;
  fill: #f7f3ed;
  text-anchor: middle;
  pointer-events: none;
  user-select: none;
}
</style>