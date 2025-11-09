<script setup lang="ts">
import { ref } from 'vue'
// 1. Import your new component
import NodesGraph from './components/NodesGraph.vue'

// 2. Create a reactive variable to hold the node count
const nodeCount = ref(3) // Start with a triangle
</script>

<template>
  <header>
    <h1>Nodes Graph</h1>
    <p>
      Based on
      <a href="https://vuejs.org/" target="_blank" rel="noopener">vuejs.org</a>
      template
    </p>
  </header>

  <main>
    <div class="controls">
      <label for="nodes">Number of Nodes: {{ nodeCount }}</label>
      <input
          id="nodes"
          v-model.number="nodeCount"
          type="range"
          min="1"
          max="20"
          step="1"
      />
    </div>

    <NodesGraph :node-count="nodeCount" />
  </main>
</template>

<style scoped>
header {
  text-align: center;
  padding: 1rem;
  border-bottom: 1px solid #ddd;
}

main {
  padding: 1rem;
}

.controls {
  max-width: 600px;
  margin: 0 auto;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 0.5rem;
  font-family: sans-serif;
  font-size: 1.2rem;
}

.controls input[type='range'] {
  width: 100%;
}
</style>