<script setup lang="ts">
import { onMounted, onUnmounted } from 'vue'
import { useNetworkStore } from '@/stores/network'

const props = withDefaults(
    defineProps<{
      refreshInterval?: number
    }>(),
    {
      refreshInterval: 5000 // Default 5 seconds
    }
)

const store = useNetworkStore()
let timerId: number | undefined = undefined

onMounted(() => {
  // Fetch immediately
  store.fileList()
  // Set up the auto-refresh timer
  timerId = window.setInterval(store.fileList, props.refreshInterval)
})

onUnmounted(() => {
  // Clean up the timer when the component is destroyed
  if (timerId) clearInterval(timerId)
})
</script>

<template>
  <div class="directory-tree-container">
    <div class="header">
      <div>
        <h3>File List</h3>
        <small>{{ store.lastFilesUpdate }}</small>
      </div>
      <button @click="store.fileList" :disabled="store.filesLoading">
        {{ store.filesLoading ? 'Refreshing...' : 'Refresh' }}
      </button>
    </div>

    <div class="tree-content">
      <div v-if="store.filesLoading && store.files.length === 0">Loading...</div>

      <div v-else-if="store.files.length === 0" class="empty-state">
        No files found.
      </div>

      <table v-else class="file-list-table">
        <thead>
        <tr>
          <th>Name</th>
          <th>Size (bytes)</th>
          <th>Start Node</th>
        </tr>
        </thead>
        <tbody>
        <tr v-for="file in store.files" :key="file.name" class="file-item">
          <td>{{ file.name }}</td>
          <td>{{ file.size }}</td>
          <td>{{ file.start }}</td>
        </tr>
        </tbody>
      </table>
    </div>
  </div>
</template>

<style scoped>
.directory-tree-container {
  display: flex;
  flex-direction: column;
  height: 100%;
  padding: 0 10px;
  min-width: 300px;
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

.header small {
  font-size: 0.8em;
  color: #555;
}

.tree-content {
  flex-grow: 1;
  overflow-y: auto;
  padding-top: 10px;
}

.empty-state {
  color: #777;
  font-style: italic;
  text-align: center;
  padding-top: 20px;
}

.file-list-table {
  width: 100%;
  border-collapse: collapse;
  font-family: monospace;
}

.file-list-table th,
.file-list-table td {
  padding: 6px 8px;
  text-align: left;
  border-bottom: 1px solid #f0f0f0;
}

.file-list-table th {
  font-weight: bold;
  background-color: #f9f9f9;
}

.file-item:hover {
  background-color: #f0f0f0;
}
</style>