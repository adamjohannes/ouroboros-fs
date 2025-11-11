<script setup lang="ts">
import { computed, onMounted, onUnmounted } from 'vue'
import { useNetworkStore } from '@/stores/network'

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

const viewBox = { width: 100, height: 100 }
const center = { x: viewBox.width / 2, y: viewBox.height / 2 }

// Radius of the polygon
const radius = 40

// Radius of the node circles
const nodeRadius = 4

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

    positions.push({ id, x, y, status })
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
  <svg
      :viewBox="`0 0 ${viewBox.width} ${viewBox.height}`"
      xmlns="http://www.w3.org/2000/svg"
      class="nodes-graph"
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
          :fill="node.status ? '#42b883' : '#e63946'"
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
</template>

<style scoped>
.nodes-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 8px 10px;
  border-bottom: 1px solid #ccc;
}

.nodes-header h3 {
  margin: 0;
}

.nodes-header small {
  color: #555;
  font-size: 0.8em;
}

.nodes-graph {
  width: 100%;
  max-width: 600px;
  margin: 1rem auto;
  display: block;
}

.lines line {
  stroke: #999;
  stroke-width: 0.5;
}

.nodes circle {
  stroke: #333;
  stroke-width: 0.5;
}

.labels text {
  font-size: 3px;
  font-family: sans-serif;
  fill: #fff;
  text-anchor: middle;
  pointer-events: none;
  user-select: none;
}
</style>