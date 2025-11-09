<script setup lang="ts">
import { computed } from 'vue'

const props = defineProps<{
  nodeCount: number
}>()

// Layout Constants

const viewBox = { width: 100, height: 100 }
const center = { x: viewBox.width / 2, y: viewBox.height / 2 }

// Radius of the polygon
const radius = 40

// Radius of the node circles
const nodeRadius = 4

// Computed Positions

/**
 * Calculates the {x, y} position for each node.
 */
const nodes = computed(() => {
  const n = props.nodeCount
  const positions: { id: number; x: number; y: number }[] = []

  // Case 1: Single node at the center
  if (n === 1) {
    positions.push({ id: 1, x: center.x, y: center.y })
    return positions
  }

  // Case 2+: Nodes at polygon vertices
  const startAngle = -Math.PI / 2
  const angleIncrement = (2 * Math.PI) / n

  for (let i = 0; i < n; i++) {
    const angle = startAngle + i * angleIncrement
    const x = center.x + radius * Math.cos(angle)
    const y = center.y + radius * Math.sin(angle)
    positions.push({ id: i + 1, x, y })
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
  <svg
      :viewBox="`0 0 ${viewBox.width} ${viewBox.height}`"
      xmlns="http://www.w3.org/2000/svg"
      class="nodes-graph"
  >
    <g class="lines">
      <line
          v-for="line in lines"
          :key="line.id"
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
.nodes-graph {
  width: 100%;
  max-width: 600px;
  margin: 2rem auto;
  display: block;
}

.lines line {
  stroke: #999;
  stroke-width: 0.5;
}

.nodes circle {
  fill: #42b883;
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