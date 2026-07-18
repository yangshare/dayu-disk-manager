<script setup lang="ts">
import { formatSize } from '../ipc/types'
defineProps<{ bytes: number }>()
</script>
<template>
  <span class="size">{{ formatSize(bytes) }}</span>
</template>
<style scoped>.size { font-variant-numeric: tabular-nums; color: #2563eb; }</style>
