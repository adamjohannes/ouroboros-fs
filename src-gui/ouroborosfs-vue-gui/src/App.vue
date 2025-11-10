<script setup lang="ts">
import { ref } from 'vue'
import NodesGraph from './components/NodesGraph.vue'
import DirectoryTree from './components/DirectoryTree.vue'
import FileUploader from './components/FileUploader.vue'

const containerRef = ref<HTMLElement | null>(null)
const leftPanelWidth = ref('50%')
const isResizing = ref(false)

// Resize Handlers

function startResize(event: MouseEvent) {
  isResizing.value = true
  window.addEventListener('mousemove', doResize)
  window.addEventListener('mouseup', stopResize)
  event.preventDefault()
}

function doResize(event: MouseEvent) {
  if (!isResizing.value || !containerRef.value) {
    return
  }

  // Calculate new width based on mouse position
  const containerRect = containerRef.value.getBoundingClientRect()
  const newWidth = event.clientX - containerRect.left

  // Set boundaries
  if (newWidth > 200 && newWidth < containerRect.width - 200) {
    leftPanelWidth.value = `${newWidth}px`
  }
}

function stopResize() {
  isResizing.value = false
  window.removeEventListener('mousemove', doResize)
  window.removeEventListener('mouseup', stopResize)
}
</script>

<template>
  <header>
    <h1>OuroborosFS Dashboard</h1>
  </header>

  <div class="resizable-container" ref="containerRef">

    <div class="panel panel-left" :style="{ flexBasis: leftPanelWidth }">
      <div class="panel-content">
        <NodesGraph />
      </div>
    </div>

    <div class="splitter" @mousedown="startResize"></div>

    <div class="panel panel-right">
      <div class="panel-content panel-right-layout">
        <DirectoryTree class="directory-tree-wrapper" />
        <FileUploader class="file-uploader-wrapper" /> </div>
    </div>

  </div>
</template>

<style>
html, body, #app {
  height: 100%;
  margin: 0;
  font-family: sans-serif;
  display: flex;
  flex-direction: column;
}

header {
  text-align: center;
  padding: 1rem;
  border-bottom: 1px solid #ddd;
  flex-shrink: 0;
}

.resizable-container {
  display: flex;
  flex-grow: 1;
  width: 100%;
  overflow: hidden;
}

.panel {
  height: 100%;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

.panel-left {
  flex-shrink: 0;
  flex-grow: 0;
}

.panel-right {
  flex-grow: 1;
  flex-shrink: 1;
}

.panel-content {
  overflow-y: auto;
  padding: 1rem;
}

.panel-right-layout {
  display: flex;
  flex-direction: column;
  height: 100%;
  padding: 0;
}
.directory-tree-wrapper {
  flex-grow: 1;
  overflow-y: auto;
  padding: 1rem;
}
.file-uploader-wrapper {
  flex-shrink: 0;
}

.splitter {
  flex-basis: 6px;
  flex-shrink: 0;
  flex-grow: 0;
  background-color: #e0e0e0;
  cursor: col-resize;
  border-left: 1px solid #ccc;
  border-right: 1px solid #ccc;
}
.splitter:hover {
  background-color: #d0d0d0;
}
</style>