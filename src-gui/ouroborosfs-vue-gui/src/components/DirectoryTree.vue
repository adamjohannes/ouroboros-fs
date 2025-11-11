<script setup lang="ts">
import {onMounted, onUnmounted} from 'vue'
import {useNetworkStore} from '@/stores/network'

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
          <th>Actions</th>
        </tr>
        </thead>
        <tbody>
        <tr v-for="file in store.files" :key="file.name" class="file-item">
          <td>{{ file.name }}</td>
          <td>{{ file.size }}</td>
          <td>{{ file.start }}</td>
          <td class="actions-cell">
            <button @click="store.filePull(file.name)" class="pull-btn">
              Pull
            </button>
          </td>
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
  padding: 0;
  min-width: 300px;
  color: #1a1a1a;
  background-color: #e8e0db;
}

.header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  border-bottom: 1px solid #333;
  padding: 8px 10px;
  flex-shrink: 0;
  background-color: #e8e0db;
}

.header h3 {
  margin: 0;
}

.header small {
  font-size: 0.8em;
  color: #555;
}

.header button {
  background-color: #1a1a1a;
  color: #f7f3ed;
  border: 1px solid #1a1a1a;
  padding: 4px 10px;
  border-radius: 4px;
  cursor: pointer;
  font-size: 0.9em;
}

.header button:hover {
  background-color: #444;
}

.header button:disabled {
  background-color: #aaa;
  color: #eee;
  cursor: not-allowed;
}


.tree-content {
  flex-grow: 1;
  overflow-y: auto;
  padding-top: 0;
  background-color: #e8e0db;
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
  padding: 6px 10px;
  text-align: left;
  border-bottom: 1px solid #dcd6cb;
  vertical-align: middle;
}

.file-list-table th {
  font-weight: bold;
  background-color: #e8e0db;
  position: sticky;
  top: 0;
  border-bottom-width: 2px;
  border-bottom-color: #333;
}

.file-item:hover {
  background-color: #dcd6cb;
}

.actions-cell {
  text-align: center;
  width: 1%;
}

.pull-btn {
  padding: 4px 10px;
  font-size: 0.9em;
  font-family: sans-serif;
  color: #f7f3ed;
  background-color: #1a1a1a;
  border: 1px solid #1a1a1a;
  border-radius: 3px;
  cursor: pointer;
  transition: background-color 0.2s;
}

.pull-btn:hover {
  background-color: #444;
}
</style>