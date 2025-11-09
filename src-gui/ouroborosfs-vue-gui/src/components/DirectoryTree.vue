<script setup lang="ts">
import { ref, onMounted, onUnmounted } from 'vue'
import TreeNode, { type Node } from './TreeNode.vue'

const props = withDefaults(
    defineProps<{
      refreshInterval?: number
    }>(),
    {
      refreshInterval: 2000
    }
)

const treeData = ref<Node | null>(null)
const isLoading = ref(false)
let timerId: number | undefined = undefined

function getMockData(): Node {
  const timestamp = new Date().toLocaleTimeString().split(' ')[0]
  return {
    id: 'root',
    name: 'src',
    type: 'folder',
    children: [
      {
        id: 'c1',
        name: 'components',
        type: 'folder',
        children: [
          { id: 'f1', name: 'NodesGraph.vue', type: 'file' },
          { id: 'f2', name: 'DirectoryTree.vue', type: 'file' }
        ]
      },
      {
        id: 'c2',
        name: 'stores',
        type: 'folder',
        children: [{ id: 'f3', name: 'counter.ts', type: 'file' }]
      },
      { id: 'f4', name: `App.vue (refreshed at ${timestamp})`, type: 'file' },
      { id: 'f5', name: 'main.ts', type: 'file' }
    ]
  }
}

async function fetchData() {
  isLoading.value = true
  console.log('Refreshing directory tree...')

  // Simulating network latency
  await new Promise((resolve) => setTimeout(resolve, 300))

  treeData.value = getMockData()
  isLoading.value = false
}

onMounted(() => {
  // 1. Fetch data on initial component load
  fetchData()

  // 2. Set up the auto-refresh timer
  timerId = window.setInterval(() => {
    fetchData()
  }, props.refreshInterval)
})

onUnmounted(() => {
  // 3. Clean up the timer when the component is destroyed
  if (timerId) {
    clearInterval(timerId)
  }
})
</script>

<template>
  <div class="directory-tree-container">
    <div class="header">
      <h3>Directory Structure</h3>
      <button @click="fetchData" :disabled="isLoading">
        {{ isLoading ? 'Refreshing...' : 'Refresh' }}
      </button>
    </div>

    <div class="tree-content">
      <div v-if="isLoading && !treeData">Loading...</div>
      <TreeNode v-if="treeData" :node="treeData" />
    </div>
  </div>
</template>

<style scoped>
.directory-tree-container {
  display: flex;
  flex-direction: column;
  height: 100%;
  padding: 0 10px;
  min-width: 250px;
}

.header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  border-bottom: 1px solid #ccc;
  padding-bottom: 8px;
  flex-shrink: 0;
}

.header h3 {
  margin: 0;
}

.tree-content {
  flex-grow: 1;
  overflow-y: auto;
  padding-top: 10px;
}
</style>